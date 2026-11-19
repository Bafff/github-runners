"""Read-only HTTP status service for the 5950x CI node (arc-runners).

Exposes ``GET /healthz`` and ``GET /status`` inside the cluster so lightweight
``route`` jobs from any repository can determine in <10 ms whether heavy ARC
jobs are active on ``k3d-5950x-ci-0``, without mounting Kubernetes API tokens
into CI runner pods or distributing cross-repository GitHub PATs.

Standard library only.
"""

from __future__ import annotations

import fnmatch
import json
import os
import signal
import socket
import ssl
import sys
import threading
import time
import urllib.error
import urllib.request
from collections.abc import Callable
from dataclasses import dataclass
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any
from urllib.parse import parse_qs, urlsplit

DEFAULT_NAMESPACE = "arc-runners"
DEFAULT_API_URL = "https://kubernetes.default.svc"
DEFAULT_TOKEN_PATH = Path("/var/run/secrets/kubernetes.io/serviceaccount/token")
DEFAULT_CA_PATH = Path("/var/run/secrets/kubernetes.io/serviceaccount/ca.crt")
DEFAULT_LOADAVG_PATH = Path("/proc/loadavg")
DEFAULT_MEMINFO_PATH = Path("/proc/meminfo")

DEFAULT_LIGHT_SUFFIXES: tuple[str, ...] = ("-route", "-light", "-deploy")
DEFAULT_LIGHT_PREFIXES: tuple[str, ...] = ("sonarqube-",)
TERMINAL_PHASES: frozenset[str] = frozenset({"Succeeded", "Failed"})


class KubernetesAPIError(RuntimeError):
    """Raised when querying the Kubernetes API fails."""


def parse_csv(value: str | None) -> tuple[str, ...]:
    """Split a comma-separated string into non-empty stripped tokens."""
    if not value:
        return ()
    return tuple(token for raw in value.split(",") if (token := raw.strip()))


def parse_query_set(query: dict[str, list[str]], key: str) -> frozenset[str]:
    """Parse repeated and/or comma-separated query values into a set."""
    values: set[str] = set()
    for raw in query.get(key, ()):
        for token in raw.split(","):
            cleaned = token.strip()
            if cleaned:
                values.add(cleaned)
    return frozenset(values)


@dataclass(frozen=True)
class LightPoolMatcher:
    """Identifies lightweight or service ARC scale sets excluded from busy state."""

    patterns: tuple[str, ...] = ()
    suffixes: tuple[str, ...] = DEFAULT_LIGHT_SUFFIXES
    prefixes: tuple[str, ...] = DEFAULT_LIGHT_PREFIXES

    @classmethod
    def from_env(cls, env: dict[str, str] | None = None) -> LightPoolMatcher:
        source = os.environ if env is None else env
        patterns = parse_csv(source.get("LIGHT_SCALE_SETS"))
        suffixes = (
            parse_csv(source["LIGHT_SCALE_SET_SUFFIXES"])
            if "LIGHT_SCALE_SET_SUFFIXES" in source
            else DEFAULT_LIGHT_SUFFIXES
        )
        prefixes = (
            parse_csv(source["LIGHT_SCALE_SET_PREFIXES"])
            if "LIGHT_SCALE_SET_PREFIXES" in source
            else DEFAULT_LIGHT_PREFIXES
        )
        return cls(patterns=patterns, suffixes=suffixes, prefixes=prefixes)

    def is_light(self, scale_set: str) -> bool:
        if not scale_set:
            return False
        if any(scale_set.endswith(suffix) for suffix in self.suffixes):
            return True
        if any(scale_set.startswith(prefix) for prefix in self.prefixes):
            return True
        return any(fnmatch.fnmatchcase(scale_set, pattern) for pattern in self.patterns)


def read_load1(path: Path = DEFAULT_LOADAVG_PATH) -> float | None:
    """Read the 1-minute load average from /proc/loadavg."""
    try:
        parts = path.read_text(encoding="utf-8").split()
        if not parts:
            return None
        return float(parts[0])
    except (OSError, ValueError):
        return None


def read_mem_available_mib(path: Path = DEFAULT_MEMINFO_PATH) -> int | None:
    """Read MemAvailable (falling back to MemFree) in MiB from /proc/meminfo."""
    try:
        text = path.read_text(encoding="utf-8")
    except OSError:
        return None

    mem_free_kib: int | None = None
    for line in text.splitlines():
        if line.startswith("MemAvailable:"):
            parts = line.split()
            if len(parts) >= 2:
                try:
                    return int(parts[1]) // 1024
                except ValueError:
                    return None
        elif line.startswith("MemFree:"):
            parts = line.split()
            if len(parts) >= 2:
                try:
                    mem_free_kib = int(parts[1])
                except ValueError:
                    mem_free_kib = None

    if mem_free_kib is not None:
        return mem_free_kib // 1024
    return None


def extract_min_runners(items: list[dict[str, Any]]) -> dict[str, int]:
    """Build a mapping from ARC scale-set name to spec.minRunners."""
    result: dict[str, int] = {}
    for item in items:
        if not isinstance(item, dict):
            continue
        spec = item.get("spec") if isinstance(item.get("spec"), dict) else {}
        meta = item.get("metadata") if isinstance(item.get("metadata"), dict) else {}
        labels = meta.get("labels") if isinstance(meta.get("labels"), dict) else {}

        raw_min = spec.get("minRunners", 0)
        if isinstance(raw_min, int) and not isinstance(raw_min, bool) and raw_min >= 0:
            min_runners = raw_min
        else:
            min_runners = 0

        keys = (
            spec.get("runnerScaleSetName"),
            labels.get("actions.github.com/scale-set-name"),
            meta.get("name"),
        )
        for key in keys:
            if isinstance(key, str) and key:
                result[key] = min_runners
    return result


def _runner_scale_set(er: dict[str, Any]) -> str:
    meta = er.get("metadata") if isinstance(er.get("metadata"), dict) else {}
    labels = meta.get("labels") if isinstance(meta.get("labels"), dict) else {}
    for key in ("actions.github.com/scale-set-name", "app.kubernetes.io/instance"):
        val = labels.get(key)
        if isinstance(val, str) and val:
            return val
    return ""


def _parse_workflow_run_id(raw: Any) -> tuple[int | None, str]:
    if isinstance(raw, int) and not isinstance(raw, bool):
        return (raw, str(raw)) if raw > 0 else (None, "")
    if isinstance(raw, str):
        cleaned = raw.strip()
        if not cleaned:
            return (None, "")
        try:
            parsed = int(cleaned)
        except ValueError:
            return (None, cleaned)
        return (parsed, cleaned) if parsed > 0 else (None, "")
    return (None, "")


def _runner_repository(status: dict[str, Any], labels: dict[str, Any]) -> str:
    repo_name = status.get("jobRepositoryName")
    if isinstance(repo_name, str) and repo_name.strip():
        return repo_name.strip()
    org = labels.get("actions.github.com/organization")
    repo = labels.get("actions.github.com/repository")
    org_s = org.strip() if isinstance(org, str) else ""
    repo_s = repo.strip() if isinstance(repo, str) else ""
    if org_s and repo_s:
        return f"{org_s}/{repo_s}"
    return repo_s


def evaluate_status(
    *,
    node: str,
    ephemeral_runners: list[dict[str, Any]],
    min_runners_by_set: dict[str, int],
    matcher: LightPoolMatcher,
    exclude_runs: frozenset[str] = frozenset(),
    exclude_jobs: frozenset[str] = frozenset(),
    load1: float | None = None,
    mem_available_mib: int | None = None,
) -> dict[str, Any]:
    """Compute the /status payload from ARC objects and node metrics."""
    heavy_jobs: list[dict[str, Any]] = []

    for er in ephemeral_runners:
        if not isinstance(er, dict):
            continue
        meta = er.get("metadata") if isinstance(er.get("metadata"), dict) else {}
        labels = meta.get("labels") if isinstance(meta.get("labels"), dict) else {}
        status = er.get("status") if isinstance(er.get("status"), dict) else {}

        scale_set = _runner_scale_set(er)
        if matcher.is_light(scale_set):
            continue

        raw_phase = status.get("phase")
        phase = raw_phase.strip() if isinstance(raw_phase, str) else ""
        if phase in TERMINAL_PHASES:
            continue

        raw_job_id = status.get("jobId")
        job_id = str(raw_job_id).strip() if raw_job_id is not None else ""

        if meta.get("deletionTimestamp") and phase != "Running" and not job_id:
            continue

        min_runners = min_runners_by_set.get(scale_set, 0)
        is_active = (phase == "Running") or bool(job_id) or (min_runners == 0)
        if not is_active:
            continue

        raw_job_name = status.get("jobDisplayName")
        job_name = raw_job_name.strip() if isinstance(raw_job_name, str) else ""
        workflow_run_id, run_id_str = _parse_workflow_run_id(status.get("workflowRunId"))

        if run_id_str and run_id_str in exclude_runs:
            continue
        if job_name and job_name in exclude_jobs:
            continue

        name = meta.get("name") if isinstance(meta.get("name"), str) else ""
        heavy_jobs.append(
            {
                "name": name,
                "scale_set": scale_set,
                "phase": phase,
                "job_id": job_id,
                "job_name": job_name,
                "repository": _runner_repository(status, labels),
                "workflow_run_id": workflow_run_id,
            }
        )

    heavy_jobs.sort(key=lambda item: (item["scale_set"], item["name"]))
    busy_by_set: dict[str, int] = {}
    for job in heavy_jobs:
        key = job["scale_set"]
        busy_by_set[key] = busy_by_set.get(key, 0) + 1

    busy_count = len(heavy_jobs)
    return {
        "node": node,
        "busy": busy_count > 0,
        "busy_count": busy_count,
        "busy_by_set": busy_by_set,
        "heavy_jobs": heavy_jobs,
        "load1": load1,
        "mem_available_mib": mem_available_mib,
    }


class KubernetesClient:
    """Minimal read-only Kubernetes REST client using Python stdlib only."""

    def __init__(
        self,
        *,
        namespace: str = DEFAULT_NAMESPACE,
        api_url: str = DEFAULT_API_URL,
        token_path: Path = DEFAULT_TOKEN_PATH,
        ca_path: Path = DEFAULT_CA_PATH,
        timeout: float = 5.0,
    ) -> None:
        self.namespace = namespace
        self.api_url = api_url.rstrip("/")
        self.token_path = token_path
        self.ca_path = ca_path
        self.timeout = timeout
        self._ssl_context: ssl.SSLContext | None = None
        if self.api_url.startswith("https://"):
            cafile = str(ca_path) if ca_path.is_file() else None
            self._ssl_context = ssl.create_default_context(cafile=cafile)

    def _read_token(self) -> str:
        try:
            token = self.token_path.read_text(encoding="utf-8").strip()
        except OSError as exc:
            raise KubernetesAPIError(f"failed to read service account token: {exc}") from exc
        if not token:
            raise KubernetesAPIError("service account token is empty")
        return token

    def get_json(self, path: str) -> dict[str, Any]:
        token = self._read_token()
        url = f"{self.api_url}{path}"
        request = urllib.request.Request(
            url,
            headers={
                "Authorization": f"Bearer {token}",
                "Accept": "application/json",
            },
            method="GET",
        )
        try:
            with urllib.request.urlopen(
                request,
                timeout=self.timeout,
                context=self._ssl_context,
            ) as response:
                payload = json.loads(response.read().decode("utf-8"))
        except urllib.error.HTTPError as exc:
            code = exc.code
            exc.close()
            raise KubernetesAPIError(f"kubernetes API {path} returned HTTP {code}") from exc
        except (urllib.error.URLError, OSError, TimeoutError) as exc:
            raise KubernetesAPIError(f"kubernetes API {path} request failed: {exc}") from exc
        except (UnicodeDecodeError, json.JSONDecodeError) as exc:
            raise KubernetesAPIError(f"kubernetes API {path} returned invalid JSON") from exc

        if not isinstance(payload, dict):
            raise KubernetesAPIError(f"kubernetes API {path} returned non-object JSON")
        return payload

    def list_autoscaling_runner_sets(self) -> list[dict[str, Any]]:
        path = f"/apis/actions.github.com/v1alpha1/namespaces/{self.namespace}/autoscalingrunnersets"
        items = self.get_json(path).get("items", [])
        return items if isinstance(items, list) else []

    def list_ephemeral_runners(self) -> list[dict[str, Any]]:
        path = f"/apis/actions.github.com/v1alpha1/namespaces/{self.namespace}/ephemeralrunners"
        items = self.get_json(path).get("items", [])
        return items if isinstance(items, list) else []


class StatusService:
    """Fetches ARC objects (with short TTL cache on runner sets) and node metrics."""

    def __init__(
        self,
        *,
        k8s: Any,
        node: str,
        matcher: LightPoolMatcher,
        ars_cache_seconds: float = 10.0,
        loadavg_path: Path = DEFAULT_LOADAVG_PATH,
        meminfo_path: Path = DEFAULT_MEMINFO_PATH,
        clock: Callable[[], float] = time.monotonic,
    ) -> None:
        self.k8s = k8s
        self.node = node
        self.matcher = matcher
        self.ars_cache_seconds = max(0.0, ars_cache_seconds)
        self.loadavg_path = loadavg_path
        self.meminfo_path = meminfo_path
        self.clock = clock
        self._lock = threading.Lock()
        self._ars_min_runners: dict[str, int] = {}
        self._ars_fetched_at: float | None = None

    def _get_min_runners(self, *, force: bool = False) -> tuple[dict[str, int], bool]:
        """Return (min_runners_by_set, was_refreshed_now)."""
        now = self.clock()
        with self._lock:
            cache_valid = (
                not force
                and self._ars_fetched_at is not None
                and self.ars_cache_seconds > 0
                and (now - self._ars_fetched_at) < self.ars_cache_seconds
            )
            if cache_valid:
                return dict(self._ars_min_runners), False

        ars_items = self.k8s.list_autoscaling_runner_sets()
        mapping = extract_min_runners(ars_items)
        fetched_at = self.clock()
        with self._lock:
            self._ars_min_runners = mapping
            self._ars_fetched_at = fetched_at
            return dict(mapping), True

    def status(
        self,
        *,
        exclude_runs: frozenset[str] = frozenset(),
        exclude_jobs: frozenset[str] = frozenset(),
    ) -> dict[str, Any]:
        min_runners, refreshed = self._get_min_runners(force=False)
        ephemeral_runners = self.k8s.list_ephemeral_runners()

        # If an EphemeralRunner belongs to a scale set not yet in the cached ARS map,
        # refresh AutoscalingRunnerSets once so newly added warm pools are known immediately.
        if not refreshed:
            unknown_heavy = any(
                (scale_set := _runner_scale_set(er))
                and not self.matcher.is_light(scale_set)
                and scale_set not in min_runners
                for er in ephemeral_runners
                if isinstance(er, dict)
            )
            if unknown_heavy:
                min_runners, _ = self._get_min_runners(force=True)

        return evaluate_status(
            node=self.node,
            ephemeral_runners=ephemeral_runners,
            min_runners_by_set=min_runners,
            matcher=self.matcher,
            exclude_runs=exclude_runs,
            exclude_jobs=exclude_jobs,
            load1=read_load1(self.loadavg_path),
            mem_available_mib=read_mem_available_mib(self.meminfo_path),
        )


def make_handler(service: StatusService) -> type[BaseHTTPRequestHandler]:
    class Handler(BaseHTTPRequestHandler):
        server_version = "ci-node-status"

        def setup(self) -> None:
            super().setup()
            self.request.settimeout(5.0)

        def _send_json(self, code: int, body: dict[str, Any]) -> None:
            data = (json.dumps(body, separators=(",", ":")) + "\n").encode("utf-8")
            self.send_response(code)
            self.send_header("Content-Type", "application/json")
            self.send_header("Cache-Control", "no-store")
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)

        def do_GET(self) -> None:
            parsed = urlsplit(self.path)
            if parsed.path == "/healthz":
                self._send_json(200, {"ok": True})
                return
            if parsed.path == "/status":
                query = parse_qs(parsed.query, keep_blank_values=False)
                exclude_runs = parse_query_set(query, "exclude_run")
                exclude_jobs = parse_query_set(query, "exclude_job")
                try:
                    payload = service.status(
                        exclude_runs=exclude_runs,
                        exclude_jobs=exclude_jobs,
                    )
                except KubernetesAPIError as exc:
                    print(f"error: {exc}", file=sys.stderr, flush=True)
                    self._send_json(503, {"error": str(exc), "node": service.node})
                    return
                self._send_json(200, payload)
                return
            self._send_json(404, {"error": "not found"})

        def log_message(self, format: str, *args: Any) -> None:
            if args and isinstance(args[0], str) and args[0].startswith("GET /healthz "):
                return
            print(f"{self.address_string()} {format % args}", flush=True)

    return Handler


class ReusableThreadingHTTPServer(ThreadingHTTPServer):
    allow_reuse_address = True
    daemon_threads = True


def build_service_from_env(env: dict[str, str] | None = None) -> StatusService:
    source = os.environ if env is None else env
    namespace = source.get("POD_NAMESPACE", DEFAULT_NAMESPACE).strip() or DEFAULT_NAMESPACE
    node = source.get("NODE_NAME", "").strip() or socket.gethostname()
    api_url = source.get("KUBERNETES_API_URL", DEFAULT_API_URL).strip() or DEFAULT_API_URL
    timeout = float(source.get("K8S_TIMEOUT_SECONDS", "5.0"))
    ars_cache_seconds = float(source.get("ARS_CACHE_SECONDS", "10.0"))
    k8s = KubernetesClient(
        namespace=namespace,
        api_url=api_url,
        token_path=Path(source.get("K8S_TOKEN_PATH", str(DEFAULT_TOKEN_PATH))),
        ca_path=Path(source.get("K8S_CA_PATH", str(DEFAULT_CA_PATH))),
        timeout=timeout,
    )
    return StatusService(
        k8s=k8s,
        node=node,
        matcher=LightPoolMatcher.from_env(source),
        ars_cache_seconds=ars_cache_seconds,
    )


def main() -> None:
    host = os.environ.get("BIND_HOST", "0.0.0.0")
    port = int(os.environ.get("PORT", "8080"))
    service = build_service_from_env()
    server = ReusableThreadingHTTPServer((host, port), make_handler(service))

    def _handle_signal(signum: int, _frame: Any) -> None:
        print(f"received signal {signum}, shutting down", flush=True)
        threading.Thread(target=server.shutdown, daemon=True).start()

    signal.signal(signal.SIGTERM, _handle_signal)
    signal.signal(signal.SIGINT, _handle_signal)

    print(
        f"ci-node-status listening on http://{host}:{port} "
        f"(node={service.node}, namespace={service.k8s.namespace})",
        flush=True,
    )
    try:
        server.serve_forever()
    finally:
        server.server_close()


if __name__ == "__main__":
    main()
