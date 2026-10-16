#!/usr/bin/env python3
"""Reproducible Scout benchmark.

Measures what the brief asks for: idle and peak RSS, CPU time, cold start,
time to first useful result, extraction accuracy, and task success rate.

Design notes, because they matter for reading the numbers:

* The client is a single persistent HTTP connection. A `curl`-per-request loop
  would fold ~50 ms of process startup into every latency sample and report
  the harness rather than the runtime.
* CPU time is read from /proc for the Scout process only, and is attributed per
  scenario by sampling before and after each one.
* Fixtures are local, so there is no network latency. They contain no
  JavaScript and no hostile markup. These numbers describe the runtime; they
  say nothing about real-site compatibility.

Usage:
    python3 bench/bench.py [--tasks N] [--out bench/results/latest.json]
"""
from __future__ import annotations

import argparse
import http.client
import json
import os
import signal
import socket
import subprocess
import sys
import threading
import time
from datetime import datetime, timezone
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
BINARY = ROOT / "target" / "release" / "scout"
FIXTURE = ROOT / "bench" / "fixture_server.py"

KEY = "bench_key_0123456789abcdefgh"
SCHEMA = {
    "item": "li.item",
    "fields": {
        "name": {"selector": ".name"},
        "price": {"selector": ".price", "type": "price"},
    },
}


def free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


class Scout:
    """Scout process plus a persistent HTTP connection to it."""

    def __init__(self, port: int):
        self.port = port
        env = {
            **os.environ,
            "SCOUT_API_KEYS": KEY,
            "SCOUT_PRIVATE_ALLOWLIST": "127.0.0.1",
            "SCOUT_LOG_LEVEL": "error",
            # The default 120/min is a production guardrail; a 200-request
            # benchmark would be throttled by it, which measures the limiter
            # rather than the runtime.
            "SCOUT_RATE_LIMIT_PER_MIN": "100000",
            "PORT": str(port),
        }
        self.proc = subprocess.Popen(
            [str(BINARY)], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
            preexec_fn=os.setsid,
        )
        self.conn = None

    @property
    def pid(self) -> int:
        return self.proc.pid

    def wait_healthy(self, timeout: float = 30.0) -> float:
        """Return milliseconds until /health first answered."""
        t0 = time.perf_counter()
        while time.perf_counter() - t0 < timeout:
            try:
                c = http.client.HTTPConnection("127.0.0.1", self.port, timeout=1)
                c.request("GET", "/health")
                if c.getresponse().status == 200:
                    c.close()
                    return (time.perf_counter() - t0) * 1000
                c.close()
            except Exception:
                time.sleep(0.005)
        raise RuntimeError("scout did not become healthy")

    def connect(self):
        self.conn = http.client.HTTPConnection("127.0.0.1", self.port, timeout=60)
        self.conn.connect()
        self.conn.sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)

    def request(self, method: str, path: str, body=None, _retry: bool = True):
        headers = {"Authorization": f"Bearer {KEY}"}
        payload = None
        if body is not None:
            payload = json.dumps(body)
            headers["Content-Type"] = "application/json"
        try:
            self.conn.request(method, path, body=payload, headers=headers)
            r = self.conn.getresponse()
            raw = r.read()
        except (http.client.BadStatusLine, http.client.RemoteDisconnected, BrokenPipeError, ConnectionResetError, OSError):
            # The server may retire an idle keep-alive connection between calls.
            # Reconnect once; this is a client concern, not a server fault.
            if not _retry:
                raise
            self.connect()
            return self.request(method, path, body, _retry=False)
        try:
            return r.status, json.loads(raw)
        except Exception:
            return r.status, None

    def rss_kb(self) -> int:
        try:
            with open(f"/proc/{self.pid}/status") as f:
                for line in f:
                    if line.startswith("VmRSS:"):
                        return int(line.split()[1])
        except OSError:
            pass
        return 0

    def cpu_ticks(self) -> int:
        try:
            with open(f"/proc/{self.pid}/stat") as f:
                parts = f.read().split()
            return int(parts[13]) + int(parts[14])
        except (OSError, IndexError):
            return 0

    def stop(self):
        try:
            os.killpg(os.getpgid(self.pid), signal.SIGTERM)
            self.proc.wait(timeout=10)
        except Exception:
            try:
                os.killpg(os.getpgid(self.pid), signal.SIGKILL)
            except Exception:
                pass


class RssSampler(threading.Thread):
    """Samples RSS on a background thread to catch the true high-water mark."""

    def __init__(self, scout: Scout, interval: float = 0.02):
        super().__init__(daemon=True)
        self.scout = scout
        self.interval = interval
        self.stop_flag = threading.Event()
        self.peak = 0

    def run(self):
        while not self.stop_flag.is_set():
            self.peak = max(self.peak, self.scout.rss_kb())
            time.sleep(self.interval)

    def stop(self):
        self.stop_flag.set()
        self.join(timeout=2)


def percentile(xs, q):
    if not xs:
        return 0
    xs = sorted(xs)
    return xs[min(len(xs) - 1, int(len(xs) * q))]


def run_scenario(scout: Scout, label: str, path: str, n: int, fixture_port: int) -> dict:
    host_base = f"http://127.0.0.1:{fixture_port}"
    status, sess = scout.request("POST", "/v1/sessions", {})
    assert status == 201, (status, sess)
    sid = sess["id"]

    cpu_before = scout.cpu_ticks()
    latencies: list[float] = []
    ok = accurate = 0
    first_result_ms = None
    statuses: dict[str, int] = {}
    errors: dict[str, int] = {}

    for i in range(n):
        t0 = time.perf_counter()
        status, body = scout.request(
            "POST",
            "/v1/extract",
            {
                "sessionId": sid,
                "url": f"{host_base}{path}",
                "strategy": "selectors",
                "schema": SCHEMA,
            },
        )
        ms = (time.perf_counter() - t0) * 1000
        latencies.append(ms)
        if first_result_ms is None:
            first_result_ms = ms

        statuses[str(status)] = statuses.get(str(status), 0) + 1
        if status != 200 or not isinstance(body, dict) or "error" in body:
            code = (body or {}).get("error", {}).get("code", "unknown") if isinstance(body, dict) else "unparsed"
            errors[code] = errors.get(code, 0) + 1
            continue
        rows = body.get("data")
        if not isinstance(rows, list) or not rows:
            errors["empty_data"] = errors.get("empty_data", 0) + 1
            continue
        ok += 1
        r = rows[0]
        price = r.get("price") or {}
        if (
            r.get("name") == "Item 0"
            and isinstance(price, dict)
            and price.get("amount") == 0.5
            and price.get("currency") == "USD"
        ):
            accurate += 1

    cpu_after = scout.cpu_ticks()
    scout.request("DELETE", f"/v1/sessions/{sid}")

    return {
        "scenario": label,
        "tasks": n,
        "successRate": round(ok / n, 4) if n else 0,
        "extractionAccuracy": round(accurate / n, 4) if n else 0,
        "firstResultMs": round(first_result_ms or 0, 1),
        "p50Ms": round(percentile(latencies, 0.50), 1),
        "p95Ms": round(percentile(latencies, 0.95), 1),
        "cpuMsPerTask": round((cpu_after - cpu_before) * 10.0 / n, 2) if n else 0,
        "statuses": statuses,
        "errors": errors,
    }


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--tasks", type=int, default=100)
    ap.add_argument("--out", default=str(ROOT / "bench" / "results" / "latest.json"))
    args = ap.parse_args()

    if not BINARY.exists():
        sys.exit(f"missing {BINARY}; run: cargo build --release")

    fixture_port = free_port()
    scout_port = free_port()

    fixture = subprocess.Popen(
        [sys.executable, str(FIXTURE), str(fixture_port)],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, preexec_fn=os.setsid,
    )
    time.sleep(0.6)

    scout = Scout(scout_port)
    sampler = None
    try:
        cold_ms = scout.wait_healthy()
        scout.connect()

        sampler = RssSampler(scout)
        sampler.start()

        time.sleep(1.0)
        idle_rss = scout.rss_kb()
        cpu_start = scout.cpu_ticks()

        results = [
            run_scenario(scout, "small-page (3 items, ~1 KB)", "/small", args.tasks, fixture_port),
            run_scenario(scout, "large-page (8000 items, ~0.8 MB)", "/large", args.tasks, fixture_port),
        ]

        cpu_end = scout.cpu_ticks()
        time.sleep(0.3)
        sampler.stop()
        peak_rss = sampler.peak
        final_rss = scout.rss_kb()

        doc = {
            "when": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
            "runtime": "scout (rust, release, lto)",
            "cpus": os.cpu_count(),
            "note": (
                "Scout process only, measured from /proc. Persistent HTTP client, so "
                "latencies exclude client startup. Local fixtures with no network "
                "latency, no JavaScript and no hostile markup: these numbers measure "
                "the runtime, not real-site compatibility."
            ),
            "coldStartMs": round(cold_ms, 1),
            "idleRssMb": round(idle_rss / 1024, 1),
            "peakRssMb": round(peak_rss / 1024, 1),
            "finalRssMb": round(final_rss / 1024, 1),
            "totalCpuSec": round((cpu_end - cpu_start) / 100.0, 2),
            "tasksPerScenario": args.tasks,
            "results": results,
        }

        Path(args.out).parent.mkdir(parents=True, exist_ok=True)
        with open(args.out, "w") as f:
            json.dump(doc, f, indent=2)

        print(json.dumps({k: v for k, v in doc.items() if k != "results"}, indent=2))
        for r in results:
            print(json.dumps(r))
        print(f"\nwritten to {args.out}")
    finally:
        if sampler:
            sampler.stop()
        scout.stop()
        try:
            os.killpg(os.getpgid(fixture.pid), signal.SIGTERM)
        except Exception:
            pass


if __name__ == "__main__":
    main()
