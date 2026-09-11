#!/usr/bin/env python3
# Copyright (c) 2026 Seonuk Kim, Human Interface Laboratory, Seoul National University
# SPDX-License-Identifier: MIT
"""Guard rails for the thing that got this machine's network suspended.

Run:  python tests/test_rate_safety.py

These tests make no real SSH connections. The subprocess call inside poll_one
is stubbed out, so what is measured is purely how often the code WOULD open a
connection.
"""
import json
import os
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request
from http.server import ThreadingHTTPServer

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, ROOT)

import hilmon                                            # noqa: E402
import ratelimit                                         # noqa: E402

CAMPUS_LIMIT = 60          # connections/sec that gets a host suspended
SAFE_TARGET = 2.0          # what we hold ourselves to
FAIL = []


def check(name, ok, detail=""):
    print("  %-4s %s%s" % ("PASS" if ok else "FAIL", name,
                           ("  -- " + detail) if detail else ""))
    if not ok:
        FAIL.append(name)


def isolated_gate(**kw):
    """A gate on a scratch file so tests never touch the real history."""
    fd, path = tempfile.mkstemp(prefix="gate-", suffix=".json")
    os.close(fd)
    os.unlink(path)
    kw.setdefault("path", path)
    return ratelimit.ConnectionGate(**kw), path


# --------------------------------------------------------------------------
print("\n1. the gate cannot be configured above the hard ceiling")
g, p = isolated_gate(min_gap=0.0, window=1.0, max_per_window=10000)
check("min_gap floored", g.min_gap >= 1.0 / ratelimit.HARD_CEILING_PER_SEC,
      "min_gap=%.2fs" % g.min_gap)
check("window cap clamped", g.max_per_window <= ratelimit.HARD_CEILING_PER_SEC * g.window,
      "%d per %.0fs" % (g.max_per_window, g.window))
check("effective rate <= hard ceiling", g.rate_per_sec() <= ratelimit.HARD_CEILING_PER_SEC,
      "%.2f/sec" % g.rate_per_sec())
check("effective rate far below campus limit", g.rate_per_sec() * 20 <= CAMPUS_LIMIT,
      "%.2f/sec vs %d/sec" % (g.rate_per_sec(), CAMPUS_LIMIT))
os.path.exists(p) and os.unlink(p)


# --------------------------------------------------------------------------
print("\n2. the gate actually paces connections")
g, p = isolated_gate(min_gap=0.2, window=1.0, max_per_window=2)
stamps = []
t0 = time.time()
for _ in range(6):
    g.acquire()
    stamps.append(time.time())
gaps = [b - a for a, b in zip(stamps, stamps[1:])]
check("every gap >= min_gap", all(x >= 0.19 for x in gaps),
      "min gap seen %.3fs" % min(gaps))
peak = ratelimit._peak_per_sec(stamps)
check("never more than max_per_window in a window", peak <= g.max_per_window,
      "peak %d/sec, cap %d" % (peak, g.max_per_window))
os.path.exists(p) and os.unlink(p)


# --------------------------------------------------------------------------
print("\n3. two threads share one ceiling (the two-collector failure)")
g, p = isolated_gate(min_gap=0.15, window=1.0, max_per_window=3)
hits = []
lock = threading.Lock()


def worker():
    for _ in range(5):
        g.acquire()
        with lock:
            hits.append(time.time())


ts = [threading.Thread(target=worker) for _ in range(2)]
[t.start() for t in ts]
[t.join() for t in ts]
peak = ratelimit._peak_per_sec(hits)
check("combined peak still under cap", peak <= g.max_per_window,
      "10 acquires across 2 threads, peak %d/sec" % peak)
os.path.exists(p) and os.unlink(p)


# --------------------------------------------------------------------------
print("\n4. a second collector is refused")
lock_a = ratelimit.SingleInstance("selftest-collector")
ok_a, _ = lock_a.acquire()
lock_b = ratelimit.SingleInstance("selftest-collector")
ok_b, who = lock_b.acquire()
check("first instance acquires", ok_a)
check("second instance refused", not ok_b, "holder: %s" % (who or "-"))
lock_a.release()
ok_c, _ = lock_b.acquire()
check("acquires again after release", ok_c)
lock_b.release()


# --------------------------------------------------------------------------
print("\n5. poll_one never opens a connection without the gate")
calls = []
real_run = hilmon.subprocess.run


class FakeDone:
    returncode = 0
    stdout = "###GPU\n0, NVIDIA Fake, GPU-x, 0, 1, 1024, 10, 100, 30, 0\n###END\n"
    stderr = ""


def fake_run(*a, **kw):
    calls.append(time.time())
    return FakeDone()


hilmon.subprocess.run = fake_run
try:
    fd, gpath = tempfile.mkstemp(prefix="gate-", suffix=".json")
    os.close(fd); os.unlink(gpath)
    cfg = {
        "ssh_user": "x", "min_ssh_interval_sec": 0.2,
        "ssh_rate_window_sec": 1.0, "ssh_max_per_window": 2,
        "servers": [{"name": "hi%d" % i, "host": "h%d" % i, "port": 22,
                     "enabled": True, "n_gpu": 1} for i in range(6)],
    }
    orig_gate_file = ratelimit.GATE_FILE
    ratelimit.GATE_FILE = gpath
    snap = hilmon.poll_all(cfg)
    ratelimit.GATE_FILE = orig_gate_file

    check("all servers polled", len(calls) == 6, "%d ssh invocations" % len(calls))
    peak = ratelimit._peak_per_sec(calls)
    check("poll_all peak under cap", peak <= 2, "peak %d/sec" % peak)
    check("snapshot built", snap["summary"]["servers_up"] == 6,
          "up=%d" % snap["summary"]["servers_up"])
finally:
    hilmon.subprocess.run = real_run
    os.path.exists(gpath) and os.unlink(gpath)


# --------------------------------------------------------------------------
print("\n6. viewers do not cause SSH connections (12+ people, many locations)")
calls.clear()
hilmon.subprocess.run = fake_run
try:
    hilmon.STATE.set({
        "generated_at": int(time.time()), "poll_interval_sec": 30, "racks": [],
        "summary": {"servers_total": 2, "servers_up": 2, "gpus_total": 2,
                    "gpus_busy": 1, "gpus_free": 1},
        "servers": [
            {"name": "hi1", "host": "10.0.0.1", "port": 22, "status": "ok",
             "gpu_model": "X", "n_gpu_expected": 1, "loc": "r", "ts": int(time.time()),
             "gpus": [{"index": 0, "name": "Fake", "util": 50, "mem_used": 1,
                       "mem_total": 2, "power": 1, "power_limit": 2, "temp": 30,
                       "fan": 0, "procs": []}]},
            {"name": "hi2", "host": "10.0.0.2", "port": 22, "status": "down",
             "gpu_model": "X", "n_gpu_expected": 1, "loc": "r", "ts": int(time.time()),
             "error": "ssh: connect to host 10.0.0.2 port 22: refused"},
        ],
    })
    httpd = ThreadingHTTPServer(("127.0.0.1", 0), hilmon.Handler)
    httpd.cfg = {"public_mode": True}
    httpd.daemon_threads = True
    threading.Thread(target=httpd.serve_forever, daemon=True).start()
    port = httpd.server_address[1]

    # 14 concurrent viewers, 10 requests each, as fast as they can.
    results = []
    rlock = threading.Lock()

    def viewer(n):
        got = 0
        for _ in range(10):
            try:
                req = urllib.request.Request(
                    "http://127.0.0.1:%d/status.json" % port)
                with urllib.request.urlopen(req, timeout=10) as r:
                    r.read()
                    got += 1
            except (urllib.error.URLError, OSError):
                pass
        with rlock:
            results.append(got)

    vt = [threading.Thread(target=viewer, args=(i,)) for i in range(14)]
    t0 = time.time()
    [t.start() for t in vt]
    [t.join() for t in vt]
    elapsed = time.time() - t0
    served = sum(results)

    check("all 140 requests served", served == 140, "%d/140 in %.2fs" % (served, elapsed))
    check("no SSH triggered by viewers", len(calls) == 0,
          "%d ssh invocations during %d requests" % (len(calls), served))

    # And the payload those viewers received leaks nothing.
    with urllib.request.urlopen(
            "http://127.0.0.1:%d/status.json" % port, timeout=10) as r:
        blob = r.read().decode("utf-8")
    leaks = [x for x in ("10.0.0.1", "10.0.0.2", "refused", '"host"', '"port"')
             if x in blob]
    check("viewer payload redacted", not leaks, "leaks: %s" % (leaks or "none"))
    httpd.shutdown()
finally:
    hilmon.subprocess.run = real_run


# --------------------------------------------------------------------------
print("\n7. the shipped config is within limits")
cfg = hilmon.load_config(os.path.join(ROOT, "servers.json"))
g = hilmon.make_gate(cfg)
n = sum(1 for s in cfg["servers"] if s.get("enabled"))
check("shipped rate <= hard ceiling", g.rate_per_sec() <= ratelimit.HARD_CEILING_PER_SEC,
      "%.2f/sec" % g.rate_per_sec())
check("shipped rate >= 20x margin", g.rate_per_sec() * 20 <= CAMPUS_LIMIT,
      "%.2f/sec vs campus %d/sec (%.0fx margin)"
      % (g.rate_per_sec(), CAMPUS_LIMIT, CAMPUS_LIMIT / g.rate_per_sec()))
print("       %d servers, full sweep %.0fs" % (n, n * g.min_gap))


print("\n" + "=" * 58)
if FAIL:
    print("FAILED: %s" % ", ".join(FAIL))
    sys.exit(1)
print("all rate-safety checks passed")
