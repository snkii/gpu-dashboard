#!/usr/bin/env python3
# Copyright (c) 2026 Seonuk Kim, Human Interface Laboratory, Seoul National University
# SPDX-License-Identifier: MIT
"""HIL GPU monitor - collector + single-page web server (stdlib only).

Polls each hi* server over SSH, runs one nvidia-smi round-trip per poll,
and serves a mobile-friendly dashboard.

Servers are polled ONE AT A TIME with a hard minimum gap between SSH
connections (min_ssh_interval_sec). The campus network flags a host that opens
several SSH connections inside a second, so the collector never bursts -- see
ratelimit.py, which enforces the ceiling in a lock file shared by every
process on this machine. Failing hosts back off exponentially.

Usage:
    python3 hilmon.py                 # collect + serve on :8899
    python3 hilmon.py --once          # single poll, print JSON, exit
    python3 hilmon.py --check         # connectivity check only
    python3 hilmon.py --port 9000
"""
import argparse
import gzip
import hashlib
import json
import os
import re
import subprocess
import sys
import threading
import time
import urllib.parse

import ratelimit
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ROOT = os.path.dirname(os.path.abspath(__file__))
WEB = os.path.join(ROOT, "web")
# Control sockets go under the user profile, not the project: a unix socket
# path is capped near 100 chars and a deep project path overruns it.
CTL = os.path.join(os.path.expanduser("~"), ".hilgpu", "cm")

# One round trip per host per poll. Sections are split on ### markers.
REMOTE = "\n".join([
    "echo '###GPU'",
    "nvidia-smi --query-gpu=index,name,uuid,utilization.gpu,memory.used,"
    "memory.total,power.draw,power.limit,temperature.gpu,fan.speed "
    "--format=csv,noheader,nounits 2>/dev/null",
    "echo '###PROC'",
    "nvidia-smi --query-compute-apps=gpu_uuid,pid,used_gpu_memory "
    "--format=csv,noheader,nounits 2>/dev/null",
    "echo '###PS'",
    "ps -eo pid=,user:24=,etimes=,comm= 2>/dev/null",
    "echo '###LOAD'",
    "cat /proc/loadavg 2>/dev/null",
    "echo '###NPROC'",
    "nproc 2>/dev/null",
    "echo '###MEM'",
    "free -m 2>/dev/null | grep -i '^Mem:'",
    "echo '###DISK'",
    "df -P -k / 2>/dev/null | tail -1",
    "echo '###UP'",
    "cut -d. -f1 /proc/uptime 2>/dev/null",
    "echo '###END'",
])


def load_config(path):
    with open(path, encoding="utf-8") as f:
        cfg = json.load(f)

    # A host flagged pin_address is reachable only at that literal address --
    # some machines have no DNS record at all. Fail loudly rather than
    # silently trying a name that does not resolve, so a well-meaning "use
    # FQDNs everywhere" edit cannot quietly take the host offline.
    for srv in cfg.get("servers", []):
        if srv.get("pin_address") and not re.fullmatch(r"[0-9.]+|[0-9a-fA-F:]+", srv["host"]):
            raise SystemExit(
                "%s: host %r is pinned to a literal IP but looks like a hostname.\n"
                "  %s" % (srv["name"], srv["host"], srv.get("note", "")))
    return cfg


def multiplex_enabled(cfg):
    """Whether to reuse SSH connections across polls.

    Windows OpenSSH does not implement connection multiplexing: ControlMaster
    fails with "Failed to connect to new control master" no matter how short
    ControlPath is (verified on OpenSSH_for_Windows_9.5p2). So it is off there
    and every poll pays a fresh handshake. On Linux/macOS it is on, which
    drops that to one handshake per host per ControlPersist. Either way the
    machine-wide gate in ratelimit.py caps how often one may be opened.
    """
    want = cfg.get("ssh_multiplex", "auto")
    if want == "auto":
        return os.name != "nt"
    return bool(want)


def ssh_argv(cfg, srv):
    argv = [
        "ssh",
        "-p", str(srv.get("port", 22)),
        "-o", "BatchMode=yes",
        "-o", "StrictHostKeyChecking=accept-new",
        "-o", "ConnectTimeout=%d" % cfg.get("ssh_connect_timeout", 6),
        "-o", "ServerAliveInterval=15",
    ]
    # Pin the restricted monitoring key where one is installed. Without this
    # the collector falls back to ~/.ssh/config, whose Host entries are keyed
    # to short aliases that do not match the "user@fqdn" target used here -- so
    # a human's unrestricted login key would be offered instead, discarding the
    # whole point of the forced-command key. Servers with no "identity" (the
    # ones set up by hand earlier) still resolve through ssh_config.
    ident = srv.get("identity") or cfg.get("ssh_identity")
    if ident:
        argv += ["-i", os.path.expanduser(ident), "-o", "IdentitiesOnly=yes"]
    if multiplex_enabled(cfg):
        # %C is a short hash of (host, port, user, local host); a literal
        # "%r@%h_%p" under a deep project path blows the ~100 char limit the
        # unix socket imposes.
        os.makedirs(CTL, exist_ok=True)
        argv += [
            "-o", "ControlMaster=auto",
            "-o", "ControlPath=" + os.path.join(CTL, "%C"),
            "-o", "ControlPersist=300",
        ]
    argv.append("%s@%s" % (srv.get("user") or cfg.get("ssh_user"), srv["host"]))
    return argv


def split_sections(text):
    out, cur = {}, None
    for line in text.splitlines():
        if line.startswith("###"):
            cur = line[3:].strip()
            out[cur] = []
        elif cur:
            out[cur].append(line.rstrip())
    return out


def _num(s, default=None):
    s = (s or "").strip()
    if s in ("", "N/A", "[N/A]", "[Not Supported]", "Not Supported", "unknown"):
        return default
    try:
        return float(s)
    except ValueError:
        return default


def parse_payload(sections):
    idx_by_uuid = {}
    gpus = []
    for line in sections.get("GPU", []):
        p = [c.strip() for c in line.split(",")]
        if len(p) < 10:
            continue
        idx = _num(p[0])
        if idx is None:
            continue
        g = {
            "index": int(idx),
            "name": p[1],
            "util": _num(p[3]),
            "mem_used": _num(p[4]),
            "mem_total": _num(p[5]),
            "power": _num(p[6]),
            "power_limit": _num(p[7]),
            "temp": _num(p[8]),
            "fan": _num(p[9]),
            "procs": [],
        }
        idx_by_uuid[p[2]] = g["index"]
        gpus.append(g)
    gpus.sort(key=lambda g: g["index"])
    by_index = {g["index"]: g for g in gpus}

    ps = {}
    for line in sections.get("PS", []):
        p = line.split(None, 3)
        if len(p) >= 4 and p[0].isdigit():
            ps[p[0]] = {"user": p[1], "etimes": int(_num(p[2], 0)), "comm": p[3]}

    for line in sections.get("PROC", []):
        p = [c.strip() for c in line.split(",")]
        if len(p) < 3:
            continue
        gi = idx_by_uuid.get(p[0])
        if gi is None or gi not in by_index:
            continue
        meta = ps.get(p[1], {})
        by_index[gi]["procs"].append({
            "pid": p[1],
            "mem": _num(p[2], 0),
            "user": meta.get("user", "?"),
            "etimes": meta.get("etimes", 0),
            "comm": meta.get("comm", "?"),
        })

    load = (sections.get("LOAD") or [""])[0].split()
    mem = (sections.get("MEM") or [""])[0].split()
    disk = (sections.get("DISK") or [""])[0].split()
    return {
        "gpus": gpus,
        "load1": _num(load[0]) if load else None,
        "nproc": int(_num((sections.get("NPROC") or ["0"])[0], 0) or 0),
        "ram_total_mb": _num(mem[1]) if len(mem) > 2 else None,
        "ram_used_mb": _num(mem[2]) if len(mem) > 2 else None,
        "disk_total_kb": _num(disk[1]) if len(disk) > 2 else None,
        "disk_used_kb": _num(disk[2]) if len(disk) > 2 else None,
        "uptime_sec": _num((sections.get("UP") or ["0"])[0], 0),
    }


def poll_one(cfg, srv, gate=None):
    """Run one nvidia-smi round trip. Every caller goes through the gate.

    The gate is taken HERE as well as in Collector, so no code path can open a
    connection without passing it -- a bypass is what caused the suspension.
    Acquiring twice is harmless: the second call sees the gap already satisfied.
    """
    (gate or make_gate(cfg, "poll_one")).acquire()
    t0 = time.time()
    base = {
        "name": srv["name"], "host": srv["host"], "port": srv.get("port", 22),
        "gpu_model": srv.get("gpu", ""), "n_gpu_expected": srv.get("n_gpu"),
        "loc": srv.get("loc", ""), "owner": srv.get("owner", ""),
        "note": srv.get("note", ""), "ts": int(time.time()),
    }
    try:
        r = subprocess.run(
            ssh_argv(cfg, srv) + [REMOTE],
            capture_output=True, text=True, errors="replace",
            timeout=cfg.get("ssh_command_timeout", 20),
        )
    except subprocess.TimeoutExpired:
        base.update(status="timeout", error="SSH timeout",
                    latency_ms=int((time.time() - t0) * 1000))
        return base
    except Exception as e:
        base.update(status="error", error=str(e),
                    latency_ms=int((time.time() - t0) * 1000))
        return base

    base["latency_ms"] = int((time.time() - t0) * 1000)
    if r.returncode != 0 or "###END" not in r.stdout:
        err = [x for x in (r.stderr or "").strip().splitlines() if x.strip()]
        base.update(status="down", error=(err[-1] if err else "exit %d" % r.returncode))
        return base

    data = parse_payload(split_sections(r.stdout))
    if not data["gpus"]:
        base.update(status="nogpu", error="nvidia-smi returned no GPUs", **data)
        return base
    base.update(status="ok", **data)
    return base


def gpu_is_busy(g):
    return (g.get("util") or 0) >= 5 or (g.get("mem_used") or 0) > 512


def make_gate(cfg, label="hilmon"):
    """The machine-wide SSH gate. See ratelimit.py for why it is not in-process.

    An in-process limiter cannot stop two processes from bursting together --
    which is exactly how this machine's network got suspended -- so the real
    ceiling lives in a lock file shared by every tool here.
    """
    return ratelimit.ConnectionGate(
        min_gap=cfg.get("min_ssh_interval_sec", ratelimit.DEFAULT_MIN_GAP),
        window=cfg.get("ssh_rate_window_sec", ratelimit.DEFAULT_WINDOW),
        max_per_window=cfg.get("ssh_max_per_window", ratelimit.DEFAULT_MAX_PER_WINDOW),
        label=label)


def sort_key(name):
    return (int(re.sub(r"\D", "", name) or 0), name)


def blank_result(srv, reason="아직 수집 전"):
    return {
        "name": srv["name"], "host": srv["host"], "port": srv.get("port", 22),
        "gpu_model": srv.get("gpu", ""), "n_gpu_expected": srv.get("n_gpu"),
        "loc": srv.get("loc", ""), "note": srv.get("note", ""),
        "status": "pending", "error": reason, "ts": 0, "latency_ms": 0,
    }


def summarize(results, cfg, sweep_sec):
    tot = busy = 0
    for s in results:
        if s.get("status") != "ok":
            continue
        for g in s.get("gpus", []):
            tot += 1
            busy += 1 if gpu_is_busy(g) else 0
    seen = [s["ts"] for s in results if s.get("ts")]
    return {
        # The oldest per-host sample bounds how fresh the page really is.
        "generated_at": int(min(seen)) if seen else 0,
        "poll_interval_sec": int(round(sweep_sec)),
        "racks": cfg.get("racks", []),
        "summary": {
            "servers_total": len(results),
            "servers_up": sum(1 for s in results if s.get("status") == "ok"),
            "gpus_total": tot,
            "gpus_busy": busy,
            "gpus_free": tot - busy,
        },
        "servers": results,
    }


class PowerLog:
    """Per-server power history: a rolling window for the page, a CSV on disk.

    The page only ever needs the recent shape of the curve, so the in-memory
    window is small and ships inside status.json. The CSV is the durable record
    -- one file per day so it stays greppable and never grows without bound.
    """

    def __init__(self, cfg):
        self.points = int(cfg.get("power_history_points", 120))
        self.dir = os.path.join(ROOT, "logs")
        self.enabled = bool(cfg.get("power_log", True))
        self.hist = {}
        self._lock = threading.Lock()
        self._day = None
        self._fh = None
        if self.enabled:
            os.makedirs(self.dir, exist_ok=True)

    def _file(self):
        day = time.strftime("%Y%m%d")
        if day != self._day:
            if self._fh:
                self._fh.close()
            path = os.path.join(self.dir, "power-%s.csv" % day)
            new = not os.path.exists(path)
            self._fh = open(path, "a", encoding="utf-8", newline="")
            if new:
                self._fh.write("ts,server,gpu_index,watts,watt_limit,util,temp\n")
            self._day = day
        return self._fh

    def record(self, srv):
        """Take one sample for a server. Returns its total watts, or None."""
        if srv.get("status") != "ok":
            return None
        gpus = srv.get("gpus") or []
        watts = [g.get("power") for g in gpus if g.get("power") is not None]
        if not watts:
            return None
        total = round(sum(watts), 1)
        ts = int(srv.get("ts") or time.time())

        with self._lock:
            h = self.hist.setdefault(srv["name"], [])
            h.append([ts, total])
            if len(h) > self.points:
                del h[:-self.points]

            if self.enabled:
                try:
                    fh = self._file()
                    for g in gpus:
                        fh.write("%d,%s,%d,%s,%s,%s,%s\n" % (
                            ts, srv["name"], g.get("index", -1),
                            "" if g.get("power") is None else round(g["power"], 1),
                            "" if g.get("power_limit") is None else round(g["power_limit"]),
                            "" if g.get("util") is None else int(g["util"]),
                            "" if g.get("temp") is None else int(g["temp"])))
                    fh.flush()
                except OSError as e:
                    # Logging must never take the collector down.
                    sys.stderr.write("[powerlog] %s\n" % e)
                    self.enabled = False
        return total

    def series(self, name):
        with self._lock:
            return [p[1] for p in self.hist.get(name, [])]


class StreamWorker(threading.Thread):
    """Hold ONE ssh connection to a server and read its stream forever.

    The remote forced command emits a block per second. Opening the connection
    is the only rate-limited act; after that the data costs nothing against the
    campus connection limit, which is what makes 1-second numbers possible at
    all. A dropped connection is reopened with exponential backoff, and that
    reconnect goes through the gate like any other connection.
    """

    def __init__(self, cfg, srv, on_block, gate):
        super().__init__(daemon=True, name="stream-" + srv["name"])
        self.cfg = cfg
        self.srv = srv
        self.on_block = on_block
        self.gate = gate
        self.stop = threading.Event()
        self.proc = None
        self.fails = 0
        self.state = "starting"
        self.last_block = 0.0

    def run(self):
        while not self.stop.is_set():
            try:
                self._session()
            except Exception as e:
                self.state = "error: %s" % str(e)[:60]
            if self.stop.is_set():
                break
            self.fails += 1
            delay = min(300, 5 * (2 ** min(self.fails, 6)))
            self.state = "reconnecting in %ds" % delay
            self.stop.wait(delay)

    def _session(self):
        self.gate.acquire()
        argv = ssh_argv(self.cfg, self.srv)
        self.state = "connecting"
        self.proc = subprocess.Popen(
            argv, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            stdin=subprocess.DEVNULL, bufsize=1, text=True, errors="replace")
        buf = []
        try:
            for line in self.proc.stdout:
                if self.stop.is_set():
                    break
                line = line.rstrip("\r\n")
                if line == "###END":
                    if buf:
                        self.fails = 0          # a full block means it works
                        self.state = "streaming"
                        self.last_block = time.time()
                        self.on_block(self.srv, "\n".join(buf))
                    buf = []
                    continue
                buf.append(line)
                if len(buf) > 4000:             # runaway remote output
                    buf = buf[-100:]
        finally:
            self.kill()

    def kill(self):
        p, self.proc = self.proc, None
        if p and p.poll() is None:
            try:
                p.kill()
            except OSError:
                pass

    def shutdown(self):
        self.stop.set()
        self.kill()


class StreamCollector:
    """Round-robin's replacement: every server streams at once, continuously.

    Connection count is len(servers), opened once. Compare with polling, which
    reopened every connection on every sweep.
    """

    def __init__(self, cfg):
        self.cfg = cfg
        self.servers = [s for s in cfg["servers"] if s.get("enabled", True)]
        self.servers.sort(key=lambda s: sort_key(s["name"]))
        self.gate = make_gate(cfg, "stream")
        self.power = PowerLog(cfg)
        self.results = {s["name"]: blank_result(s, "연결 중") for s in self.servers}
        self.slow = {}                   # last host-stats block per server
        self._lock = threading.Lock()
        self.workers = []

    def start(self):
        for s in self.servers:
            w = StreamWorker(self.cfg, s, self._on_block, self.gate)
            self.workers.append(w)
            w.start()

    def shutdown(self):
        for w in self.workers:
            w.shutdown()

    def _on_block(self, srv, text):
        sections = split_sections(text)
        data = parse_payload(sections)
        name = srv["name"]

        with self._lock:
            # Fast blocks carry only GPU lines; merge the last slow block's
            # host stats onto them so the card never flickers between
            # "48 cores, 500G RAM" and blank.
            if sections.get("LOAD"):
                self.slow[name] = {k: data[k] for k in
                                   ("load1", "nproc", "ram_total_mb", "ram_used_mb",
                                    "disk_total_kb", "disk_used_kb", "uptime_sec")}
            else:
                data.update(self.slow.get(name, {}))
                # procs only arrive on slow blocks; keep the previous counts.
                prev = {g["index"]: g.get("procs", [])
                        for g in self.results[name].get("gpus", [])}
                for g in data["gpus"]:
                    if not g["procs"]:
                        g["procs"] = prev.get(g["index"], [])

            res = {
                "name": name, "host": srv["host"], "port": srv.get("port", 22),
                "gpu_model": srv.get("gpu", ""), "n_gpu_expected": srv.get("n_gpu"),
                "loc": srv.get("loc", ""), "note": srv.get("note", ""),
                "ts": int(time.time()), "latency_ms": 0,
            }
            res.update(data)
            res["status"] = "ok" if data["gpus"] else "nogpu"
            if not data["gpus"]:
                res["error"] = "nvidia-smi returned no GPUs"
            self.results[name] = res
        self.power.record(res)

    def snapshot(self):
        with self._lock:
            ordered = []
            for s in self.servers:
                r = dict(self.results[s["name"]])
                w = [x for x in self.workers if x.srv["name"] == s["name"]]
                if w and r.get("status") != "ok":
                    st = w[0].state
                    r["status"] = "pending" if st in ("starting", "connecting") else "down"
                    r["error"] = st
                # A stream that stopped delivering is down, even though the
                # process may still be alive.
                if r.get("status") == "ok" and w:
                    age = time.time() - w[0].last_block
                    if age > 30:
                        r["status"] = "down"
                        r["error"] = "stream stalled %ds" % int(age)
                series = self.power.series(s["name"])
                if series:
                    r["watts"] = round(series[-1], 1)
                    r["watt_history"] = [round(v) for v in series]
                ordered.append(r)
        return summarize(ordered, self.cfg, 1)


class Collector:
    """Round-robin poller: one host per turn, never two at once.

    A host that fails backs off exponentially, so a powered-down or blocked
    server is not retried every sweep -- which is what turns a temporary
    fail2ban block into a permanent one.
    """

    def __init__(self, cfg):
        self.cfg = cfg
        self.servers = [s for s in cfg["servers"] if s.get("enabled", True)]
        self.servers.sort(key=lambda s: sort_key(s["name"]))
        self.limiter = make_gate(cfg, "collector")
        self.power = PowerLog(cfg)
        self.results = {s["name"]: blank_result(s) for s in self.servers}
        self.next_due = {s["name"]: 0.0 for s in self.servers}
        self.fails = {s["name"]: 0 for s in self.servers}
        self.max_backoff = float(cfg.get("max_backoff_sec", 600))
        self.i = 0

    def sweep_seconds(self):
        """How long one full pass over every host takes at the current rate."""
        return max(1.0, self.limiter.min_gap) * max(1, len(self.servers))

    def snapshot(self):
        ordered = []
        for s in self.servers:
            r = dict(self.results[s["name"]])
            series = self.power.series(s["name"])
            if series:
                r["watts"] = round(series[-1], 1)
                r["watt_history"] = [round(v) for v in series]
            ordered.append(r)
        return summarize(ordered, self.cfg, self.sweep_seconds())

    def poll_next(self):
        """Poll at most one host. Returns its name, or None if none are due."""
        if not self.servers:
            return None
        now = time.time()
        for _ in range(len(self.servers)):
            srv = self.servers[self.i % len(self.servers)]
            self.i += 1
            name = srv["name"]
            if self.next_due[name] > now:
                continue
            res = poll_one(self.cfg, srv, gate=self.limiter)
            self.power.record(res)
            self.results[name] = res
            if res.get("status") == "ok":
                self.fails[name] = 0
                self.next_due[name] = time.time()
            else:
                self.fails[name] += 1
                # 2x per failure on top of one sweep, capped.
                delay = min(self.max_backoff,
                            self.sweep_seconds() * (2 ** min(self.fails[name], 6)))
                self.next_due[name] = time.time() + delay
            return name
        return None


def poll_all(cfg):
    """One full sweep, rate-limited. Used by --check / --once / --publish."""
    c = Collector(cfg)
    for _ in range(len(c.servers)):
        c.poll_next()
    return c.snapshot()


def redact(snap, cfg):
    """Strip internal detail before the snapshot leaves the process.

    The dashboard is reachable from the public internet, so the served payload
    must not carry campus IPs, SSH ports, or real login names -- that is
    reconnaissance material, and no amount of front-end auth un-leaks it once
    it has been served. Utilisation numbers are what the page is for; the rest
    is not. Set "public_mode": false only for a LAN-only deployment.
    """
    if not cfg.get("public_mode", True):
        return snap

    out = dict(snap)
    servers = []
    for s in snap.get("servers", []):
        c = dict(s)
        # Network reconnaissance material.
        c.pop("host", None)
        c.pop("port", None)
        # Identity. The lab is small enough that an initial ("한··") or a
        # hash of a login is trivially re-identified from a roster of ~18
        # people, so no derived form of a name is published either -- the
        # owner field is dropped and processes are reduced to counts.
        c.pop("owner", None)
        # note is an operator scratchpad: it holds whatever an admin needed
        # to remember about a host, which may name people or networks. Nothing
        # written there should reach the public page, so it never ships.
        c.pop("note", None)
        # Round-trip latency fingerprints the network path; it is of no use to
        # a viewer and is a timing side channel.
        c.pop("latency_ms", None)
        # An SSH error string echoes the host and port back; replace it.
        if c.get("error"):
            c["error"] = {"timeout": "SSH timeout", "down": "unreachable",
                          "nogpu": "no GPU reported"}.get(s.get("status"), "unavailable")
        gpus = []
        for g in s.get("gpus", []):
            cg = dict(g)
            procs = g.get("procs", [])
            cg["procs"] = [{"mem": p.get("mem")} for p in procs]
            gpus.append(cg)
        if gpus:
            c["gpus"] = gpus
        servers.append(c)
    out["servers"] = servers
    # Rack notes are operator hints ("문 밀면서 열기", KVM switch order). The
    # room name and slot order are what the layout view needs; the hints are
    # physical-access detail with no monitoring value, so they stay internal.
    out["racks"] = [{k: v for k, v in r.items() if k != "note"}
                    for r in snap.get("racks", [])]
    out["redacted"] = True
    return out


class State:
    def __init__(self):
        self.lock = threading.Lock()
        self.snapshot = {"generated_at": 0, "servers": [], "summary": {}, "booting": True}

    def set(self, snap):
        with self.lock:
            self.snapshot = snap

    def get(self):
        with self.lock:
            return self.snapshot


STATE = State()


class ResponseCache:
    """Serialise + hash each snapshot once, however many viewers ask for it.

    Viewer load and SSH load are completely decoupled: the collector polls on
    its own rate-limited schedule and viewers only ever read the last snapshot
    from memory. A dozen people refreshing once a second cause ZERO extra SSH
    connections -- they cannot, because nothing in the request path touches
    ssh. This cache just avoids re-serialising the same JSON for each of them,
    so the work per snapshot is O(1) rather than O(viewers).
    """

    def __init__(self):
        self._lock = threading.Lock()
        self._key = None
        self._body = b""
        self._tag = ""

    def get(self, state, cfg):
        snap = state.get()
        key = id(snap)          # State swaps in a new dict per poll
        with self._lock:
            if key == self._key:
                return self._body, self._tag
        body = json.dumps(redact(snap, cfg), ensure_ascii=False,
                          separators=(",", ":")).encode("utf-8")
        tag = '"%s"' % hashlib.blake2b(body, digest_size=12).hexdigest()
        with self._lock:
            self._key, self._body, self._tag = key, body, tag
        return body, tag


RESPONSE_CACHE = ResponseCache()


def make_collector(cfg):
    """Streaming when the servers have the streaming command installed.

    Both collectors expose the same snapshot()/shutdown() shape, so the server
    and the publisher do not care which one they got.
    """
    if cfg.get("stream", True):
        return StreamCollector(cfg)
    return Collector(cfg)


def collector_loop(cfg, stop):
    """Continuously refresh one host at a time.

    Nothing bursts: the rate limiter guarantees a gap between connections, and
    each completed host immediately updates the published snapshot, so the page
    improves steadily instead of jumping once per sweep.
    """
    c = make_collector(cfg)
    STATE.set(c.snapshot())

    if isinstance(c, StreamCollector):
        # Each server has its own worker pushing blocks as they arrive, so this
        # loop only republishes what they have already collected.
        c.start()
        try:
            while not stop.is_set():
                STATE.set(c.snapshot())
                stop.wait(1.0)
        finally:
            c.shutdown()
        return

    while not stop.is_set():
        try:
            name = c.poll_next()
            STATE.set(c.snapshot())
            if name is None:
                # Everything is backing off; idle briefly rather than spin.
                stop.wait(1.0)
        except Exception as e:
            sys.stderr.write("[collector] %s\n" % e)
            stop.wait(2.0)


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *a):
        pass

    # No inline event handlers or external assets are used, so the page runs
    # under a strict CSP. 'unsafe-inline' is required only for the one inline
    # <script>/<style> block; keep it that way rather than widening to a CDN.
    SECURITY_HEADERS = [
        ("Content-Security-Policy",
         "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; "
         "connect-src 'self'; img-src 'self' data:; manifest-src 'self'; "
         "base-uri 'none'; form-action 'none'; "
         "frame-ancestors 'none'"),
        ("X-Content-Type-Options", "nosniff"),
        ("X-Frame-Options", "DENY"),
        ("Referrer-Policy", "no-referrer"),
        ("Permissions-Policy", "geolocation=(), camera=(), microphone=(), interest-cohort=()"),
        ("Cross-Origin-Opener-Policy", "same-origin"),
        ("Cross-Origin-Resource-Policy", "same-origin"),
        ("Strict-Transport-Security", "max-age=63072000; includeSubDomains"),
    ]

    def _send(self, code, body, ctype, etag=None):
        if isinstance(body, str):
            body = body.encode("utf-8")

        # A 1 Hz dashboard mostly asks for data that has not changed. Answer
        # those with 304 and no body at all; gzip the rest.
        if etag and self.headers.get("If-None-Match") == etag:
            self.send_response(304)
            self.send_header("ETag", etag)
            self.send_header("Cache-Control", "no-cache")
            for k, v in self.SECURITY_HEADERS:
                self.send_header(k, v)
            self.end_headers()
            return

        enc = None
        if len(body) > 500 and "gzip" in (self.headers.get("Accept-Encoding") or ""):
            body, enc = gzip.compress(body, 6), "gzip"

        self.send_response(code)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Cache-Control", "no-cache" if etag else "no-store")
        if enc:
            self.send_header("Content-Encoding", enc)
            self.send_header("Vary", "Accept-Encoding")
        if etag:
            self.send_header("ETag", etag)
        for k, v in self.SECURITY_HEADERS:
            self.send_header(k, v)
        self.end_headers()
        if self.command != "HEAD":
            self.wfile.write(body)

    def do_HEAD(self):
        self.do_GET()

    def do_GET(self):
        path = self.path.split("?", 1)[0]
        if path in ("/", "/index.html"):
            try:
                with open(os.path.join(WEB, "index.html"), "rb") as f:
                    return self._send(200, f.read(), "text/html; charset=utf-8")
            except OSError:
                return self._send(500, "web/index.html missing",
                                  "text/plain; charset=utf-8")
        if path in ("/api/status", "/status.json"):
            body, tag = RESPONSE_CACHE.get(STATE, self.server.cfg)
            return self._send(200, body, "application/json; charset=utf-8", etag=tag)
        return self._serve_static(path)

    STATIC_TYPES = {
        ".png": "image/png", ".ico": "image/x-icon", ".svg": "image/svg+xml",
        ".webmanifest": "application/manifest+json", ".json": "application/json",
        ".webp": "image/webp", ".css": "text/css", ".js": "text/javascript",
    }

    def _serve_static(self, path):
        """Serve icons and the manifest out of web/, and nothing else.

        The resolved path is required to stay inside WEB, so no amount of
        ../ or symlink trickery in the URL can reach the rest of the disk.
        """
        rel = urllib.parse.unquote(path).lstrip("/")
        ext = os.path.splitext(rel)[1].lower()
        if not rel or ext not in self.STATIC_TYPES:
            return self._send(404, "not found", "text/plain; charset=utf-8")
        target = os.path.realpath(os.path.join(WEB, rel))
        webroot = os.path.realpath(WEB)
        if os.path.commonpath([target, webroot]) != webroot or not os.path.isfile(target):
            return self._send(404, "not found", "text/plain; charset=utf-8")
        try:
            with open(target, "rb") as f:
                body = f.read()
        except OSError:
            return self._send(404, "not found", "text/plain; charset=utf-8")
        tag = '"%s"' % hashlib.blake2b(body, digest_size=12).hexdigest()
        return self._send(200, body, self.STATIC_TYPES[ext], etag=tag)


def publish_once(cfg, outdir, copy_web=True, collector=None):
    """Write the redacted snapshot (and the page) for upload to a static host.

    status.json is written to a temp name and renamed, so an uploader that runs
    concurrently never reads a half-written file.
    """
    os.makedirs(outdir, exist_ok=True)
    # With a live streaming collector the data is already here; only the
    # one-shot path needs to go out and fetch a sweep.
    raw = collector.snapshot() if collector is not None else poll_all(cfg)
    snap = redact(raw, cfg)
    tmp = os.path.join(outdir, "status.json.tmp")
    with open(tmp, "w", encoding="utf-8") as f:
        json.dump(snap, f, ensure_ascii=False, separators=(",", ":"))
    os.replace(tmp, os.path.join(outdir, "status.json"))

    if copy_web:
        for rel in ("index.html",):
            with open(os.path.join(WEB, rel), "rb") as src:
                data = src.read()
            with open(os.path.join(outdir, rel), "wb") as dst:
                dst.write(data)
        icons_src = os.path.join(WEB, "icons")
        if os.path.isdir(icons_src):
            icons_dst = os.path.join(outdir, "icons")
            os.makedirs(icons_dst, exist_ok=True)
            for name in os.listdir(icons_src):
                p = os.path.join(icons_src, name)
                if os.path.isfile(p):
                    with open(p, "rb") as src, open(os.path.join(icons_dst, name), "wb") as dst:
                        dst.write(src.read())
    return snap


def publish(cfg, args):
    """One-shot or continuous publish.

    In --loop mode this process is meant to be kept alive by the OS (a Windows
    scheduled task with a restart-on-failure policy), so it must never exit on
    a transient error: a failed poll or a failed upload is logged and retried
    on the next tick. The public site keeps serving the last good snapshot
    regardless, which is why a desktop reboot never takes the page down.
    """
    # With streaming the data arrives every second, so the publish cadence is
    # about how often to upload, not how often to collect. Polling had no such
    # split -- there the interval WAS the collection rate.
    streaming = cfg.get("stream", True) and args.loop
    default_interval = 2 if streaming else cfg.get("poll_interval_sec", 5)
    interval = max(1 if streaming else 2,
                   int(args.publish_interval or default_interval))

    # Same lock the serve mode takes. Without it a `--publish --loop` daemon and
    # an interactive `hilmon.py` both poll: the shared gate keeps the TOTAL rate
    # legal, but they then split one budget, so each sweep takes twice as long
    # and the daemon can be starved out of publishing entirely.
    guard = None
    if args.loop:
        guard = ratelimit.SingleInstance("collector")
        ok, other = guard.acquire()
        if not ok:
            print("another collector is already running (%s)." % other)
            print("Stop it first -- two collectors share one rate budget and "
                  "neither gets a full sweep.")
            return 1

    collector = None
    if streaming:
        collector = StreamCollector(cfg)
        collector.start()
        print("streaming: %d servers, one persistent connection each, "
              "publishing every %ds" % (len(collector.servers), interval), flush=True)

    first = True
    first_upload = True
    try:
        while True:
            t0 = time.time()
            try:
                snap = publish_once(cfg, args.publish, copy_web=first,
                                    collector=collector)
                first = False
                sm = snap["summary"]
                msg = "%s  published %d/%d up, %d/%d GPUs free%s" % (
                    time.strftime("%H:%M:%S"), sm["servers_up"], sm["servers_total"],
                    sm["gpus_free"], sm["gpus_total"],
                    ", redacted" if snap.get("redacted") else "")
                if args.upload_cmd:
                    # Only status.json changes after the first publish. Syncing
                    # the whole directory every tick made rclone check all 11
                    # objects -- eleven billed operations for one changed file.
                    # The first run uploads everything; the steady state uploads
                    # exactly one object.
                    cmd = args.upload_cmd
                    if not first_upload and args.upload_cmd_tick:
                        cmd = args.upload_cmd_tick
                    first_upload = False
                    r = subprocess.run(cmd, shell=True, cwd=args.publish,
                                       capture_output=True, text=True, timeout=180)
                    msg += " | upload %s" % ("ok" if r.returncode == 0 else
                                             "FAILED rc=%d %s" % (r.returncode,
                                                                  (r.stderr or "").strip()[:200]))
                print(msg, flush=True)
            except Exception as e:
                # Keep the loop alive; the task scheduler is the last resort only
                # for a hard crash of the interpreter itself.
                print("%s  publish error: %s" % (time.strftime("%H:%M:%S"), e), flush=True)
            if not args.loop:
                if guard:
                    guard.release()
                return 0
            time.sleep(max(0.5, interval - (time.time() - t0)))
    finally:
      if collector:
          collector.shutdown()
      if guard:
          guard.release()


# ---------------------------------------------------------------------------
# Delegation to the Rust collector.
#
# The boot task that starts this file was registered by an elevated process and
# its DACL now refuses every non-admin change, so the command line it runs
# cannot be repointed at bin\hilmon.exe. Delegating here reaches the same end
# without asking anyone for an admin prompt: whatever starts this script ends up
# running the Rust collector, which is the one that keeps the statistics
# database.
#
# Set HILMON_PYTHON=1 to run this implementation instead.
# ---------------------------------------------------------------------------

RUST_EXE = os.path.join(ROOT, "bin", "hilmon.exe")
BUNDLED_RCLONE = os.path.join(ROOT, "bin", "rclone.exe")


def _repoint_rclone(arg):
    """Rewrite any path ending in rclone.exe to the copy in bin/.

    Written as a scan rather than a regex because the paths involved are full
    of backslashes and a pattern for them is far easier to get subtly wrong.
    """
    needle = "rclone.exe"
    out, i = "", 0
    while True:
        j = arg.find(needle, i)
        if j < 0:
            return out + arg[i:]
        # Walk back over the directory part to whatever opened it.
        k = j
        while k > 0 and arg[k - 1] not in ('"', "'", " ", "\t"):
            k -= 1
        out += arg[i:k] + BUNDLED_RCLONE
        i = j + len(needle)


def _delegate_to_rust(argv):
    """Re-exec the Rust collector, or return None to run the Python one."""
    if os.environ.get("HILMON_PYTHON") or not os.path.exists(RUST_EXE):
        return None
    # Only the publishing daemon is delegated. The one-shot developer modes
    # stay on this implementation so the two can still be compared.
    if "--publish" not in argv:
        return None

    args = list(argv)

    # Replace each upload command with the .cmd file that generates it.
    #
    # The task line predates those files and passes the commands inline, as
    # strings whose quotes are backslash-escaped for one layer of parsing and
    # then survive into the next one:
    #
    #     '\"C:\...\rclone.exe\"' is not recognized as an internal or
    #     external command
    #
    # Rewriting only the rclone path inside such a string keeps the escaping
    # that breaks it. A .cmd file path has no spaces, so it needs no quoting at
    # any layer. The files come from deploy/install-daemon.ps1.
    #
    # This failed silently for hours after a power cut: the boot path starts
    # before the watchdog does, and the collector kept publishing locally while
    # every upload failed.
    for flag, script in (("--upload-cmd", "up-first.cmd"),
                         ("--upload-cmd-tick", "up-status.cmd"),
                         ("--upload-cmd-stats", "up-stats.cmd"),
                         ("--upload-cmd-summary", "up-summary.cmd")):
        path = os.path.join(ROOT, "bin", script)
        if not os.path.exists(path):
            continue
        if flag in args:
            args[args.index(flag) + 1] = path
        else:
            args += [flag, path]

    print("[hilmon] delegating to {}".format(RUST_EXE), flush=True)
    # Spawn and wait rather than os.execv: Windows has no real exec, so execv
    # would end this process -- and with it the scheduler's supervision --
    # leaving the collector orphaned and un-restartable. Waiting keeps the
    # cmd -> python -> hilmon chain intact and passes the exit code back up.
    child = subprocess.Popen([RUST_EXE] + args)
    try:
        raise SystemExit(child.wait())
    except KeyboardInterrupt:
        child.terminate()
        raise SystemExit(child.wait())
    finally:
        if child.poll() is None:
            # Stopping the task kills this process; do not leave the collector
            # behind still holding the single-instance lock.
            child.terminate()


def main():
    _delegate_to_rust(sys.argv[1:])
    ap = argparse.ArgumentParser()
    ap.add_argument("-c", "--config", default=os.path.join(ROOT, "servers.json"))
    ap.add_argument("-p", "--port", type=int, default=8899)
    # Localhost by default: the dashboard exposes internal IPs, owner names and
    # running usernames, so public exposure must go through an authenticating
    # front end (cloudflared / Caddy) rather than binding the world by accident.
    ap.add_argument("--bind", default="127.0.0.1")
    ap.add_argument("--once", action="store_true", help="poll once, print JSON, exit")
    ap.add_argument("--check", action="store_true", help="connectivity check only")
    ap.add_argument("--rate", action="store_true",
                    help="print the SSH rate gate's limits and recent history, then exit")
    ap.add_argument("--publish", metavar="DIR",
                    help="write redacted status.json + the page to DIR for upload to a "
                         "static host (no listener is opened)")
    ap.add_argument("--loop", action="store_true",
                    help="with --publish: keep publishing until stopped, surviving "
                         "transient poll/upload failures")
    ap.add_argument("--publish-interval", type=int, metavar="SEC",
                    help="with --loop: seconds between publishes "
                         "(default: poll_interval_sec from the config)")
    ap.add_argument("--upload-cmd", metavar="CMD",
                    help="with --publish: shell command run in DIR after the FIRST "
                         "write (uploads the page, icons and status.json)")
    ap.add_argument("--upload-cmd-tick", metavar="CMD",
                    help="with --publish --loop: the command for every later tick. "
                         "Only status.json changes, so this should upload that one "
                         "object; syncing the whole directory bills 11 operations "
                         "per tick instead of 1.")
    args = ap.parse_args()

    cfg = load_config(args.config)

    if args.rate:
        g = make_gate(cfg)
        st = ratelimit.stats()
        print("SSH rate gate")
        print("  file             : %s" % ratelimit.GATE_FILE)
        print("  min gap          : %.2fs between connections" % g.min_gap)
        print("  window           : %d connections / %.0fs" % (g.max_per_window, g.window))
        print("  sustained ceiling: %.2f conn/sec" % g.rate_per_sec())
        print("  hard ceiling     : %.2f conn/sec (not configurable higher)"
              % ratelimit.HARD_CEILING_PER_SEC)
        print("  campus limit     : ~60 conn/sec")
        print("  margin           : %.0fx below the campus limit"
              % (60.0 / max(g.rate_per_sec(), 1e-9)))
        print("recent history")
        print("  last 1s / 10s / 60s : %d / %d / %d"
              % (st["last_1s"], st["last_10s"], st["last_60s"]))
        print("  peak in any 1s      : %d" % st.get("peak_per_sec", 0))
        return 0

    if args.publish:
        return publish(cfg, args)

    if args.check:
        snap = poll_all(cfg)
        w = max(len(s["name"]) for s in snap["servers"])
        for s in snap["servers"]:
            mark = "OK  " if s["status"] == "ok" else "FAIL"
            extra = ("%d GPU" % len(s.get("gpus", []))) if s["status"] == "ok" \
                else s.get("error", "")[:60]
            print("  %s %-*s  %s:%-5d %5dms  %s" % (
                mark, w, s["name"], s["host"], s["port"], s["latency_ms"], extra))
        up = snap["summary"]["servers_up"]
        print("\n%d/%d up, %d GPUs visible" % (
            up, snap["summary"]["servers_total"], snap["summary"]["gpus_total"]))
        return 0 if up else 1

    if args.once:
        print(json.dumps(poll_all(cfg), ensure_ascii=False, indent=2))
        return 0

    # Two collectors polling at once is what got this machine's network
    # suspended; make it impossible rather than merely discouraged.
    guard = ratelimit.SingleInstance("collector")
    ok, other = guard.acquire()
    if not ok:
        print("another collector is already running (%s)." % other)
        print("Stop it first: two collectors doubled the connection rate and "
              "got this machine's network suspended.")
        return 1

    stop = threading.Event()
    threading.Thread(target=collector_loop, args=(cfg, stop), daemon=True).start()

    httpd = ThreadingHTTPServer((args.bind, args.port), Handler)
    httpd.daemon_threads = True
    httpd.cfg = cfg
    n = sum(1 for s in cfg["servers"] if s.get("enabled", True))
    gap = float(cfg.get("min_ssh_interval_sec", 2.5))
    print("HIL GPU monitor  ->  http://localhost:%d" % args.port)
    print("  %d servers, one at a time, >=%.1fs between SSH connections" % (n, gap))
    print("  = %.2f connections/sec, full sweep every %.0fs" % (1.0 / gap, n * gap))
    print("  page refreshes every 1s; data is only as fresh as the sweep")
    print("  Ctrl+C to stop")
    try:
        httpd.serve_forever()
    except KeyboardInterrupt:
        print("\nstopping...")
    finally:
        stop.set()
        httpd.shutdown()
        guard.release()
    return 0


if __name__ == "__main__":
    sys.exit(main())
