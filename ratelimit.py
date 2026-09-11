#!/usr/bin/env python3
# Copyright (c) 2026 Seonuk Kim, Human Interface Laboratory, Seoul National University
# SPDX-License-Identifier: MIT
"""Machine-wide gate on outbound SSH connections.

Why this exists: the campus network suspends a host that opens more than ~60
SSH connections in a second. It already happened once here, because two
collector processes were running at the same time and each had its own
in-process limiter -- neither was wrong on its own, and together they burst.
An in-process limit cannot prevent that. So the gate lives in a FILE, shared
by every tool in this project and every process on this machine.

Two independent rules, both enforced:

  1. MIN_GAP seconds between any two connections.
  2. no more than MAX_PER_WINDOW connections in any WINDOW seconds.

Rule 2 is the one that matters for the campus limit; rule 1 keeps the pace
even. HARD_CEILING_PER_SEC is a floor under any configuration: the gate
refuses to be configured looser than that, so a future edit to a config file
cannot quietly remove the protection.

Every acquire() is serialised by an exclusive lock on the state file, and the
waiting happens while holding it -- that is what makes the limit hold ACROSS
processes rather than per process.
"""
import json
import os
import sys
import time

if os.name == "nt":
    import msvcrt
else:
    import fcntl

STATE_DIR = os.path.join(os.path.expanduser("~"), ".hilgpu")
GATE_FILE = os.path.join(STATE_DIR, "ssh-gate.json")

# The campus threshold is ~60/sec. Everything here is deliberately far below
# it: bursting anywhere near the limit is what got this machine suspended.
HARD_CEILING_PER_SEC = 2.0     # never allow a config looser than this
DEFAULT_MIN_GAP = 2.5          # seconds between connections
DEFAULT_WINDOW = 10.0          # sliding window length
DEFAULT_MAX_PER_WINDOW = 5     # -> 0.5/sec sustained


def _lock(fh, blocking=True):
    """Take an exclusive lock on byte 0 of the file.

    On Windows msvcrt.locking() locks a range starting at the CURRENT file
    position, and a file opened "a+" starts positioned at EOF -- so two
    processes would lock two different byte ranges and both "succeed",
    silently giving up all mutual exclusion. Seeking to 0 first is what makes
    this a real cross-process lock.
    """
    if os.name == "nt":
        while True:
            try:
                fh.seek(0)
                msvcrt.locking(fh.fileno(),
                               msvcrt.LK_LOCK if blocking else msvcrt.LK_NBLCK, 1)
                return True
            except OSError:
                if not blocking:
                    return False
                time.sleep(0.05)
    else:
        try:
            fcntl.flock(fh.fileno(),
                        fcntl.LOCK_EX if blocking else fcntl.LOCK_EX | fcntl.LOCK_NB)
            return True
        except OSError:
            if not blocking:
                return False
            raise


def _unlock(fh):
    if os.name == "nt":
        try:
            fh.seek(0)
            msvcrt.locking(fh.fileno(), msvcrt.LK_UNLCK, 1)
        except OSError:
            pass
    else:
        fcntl.flock(fh.fileno(), fcntl.LOCK_UN)


class ConnectionGate:
    def __init__(self, min_gap=DEFAULT_MIN_GAP, window=DEFAULT_WINDOW,
                 max_per_window=DEFAULT_MAX_PER_WINDOW, path=GATE_FILE,
                 label="ssh"):
        self.window = max(1.0, float(window))
        self.max_per_window = max(1, int(max_per_window))
        self.label = label
        self.path = path

        # Clamp against the hard ceiling, whatever the caller asked for.
        ceiling_gap = 1.0 / HARD_CEILING_PER_SEC
        self.min_gap = max(float(min_gap), ceiling_gap)
        allowed = self.window * HARD_CEILING_PER_SEC
        if self.max_per_window > allowed:
            self.max_per_window = int(allowed)

        os.makedirs(STATE_DIR, exist_ok=True)

    def rate_per_sec(self):
        """The sustained ceiling this configuration actually permits."""
        return min(1.0 / self.min_gap, self.max_per_window / self.window)

    def acquire(self, timeout=120.0):
        """Block until another connection is permitted. Returns waited seconds."""
        started = time.time()
        waited = 0.0
        while True:
            with open(self.path, "a+", encoding="utf-8") as fh:
                _lock(fh)
                try:
                    fh.seek(0)
                    raw = fh.read().strip()
                    try:
                        hist = [float(x) for x in json.loads(raw)] if raw else []
                    except (ValueError, TypeError):
                        hist = []

                    now = time.time()
                    hist = [t for t in hist if now - t < self.window * 3][-200:]

                    wait = 0.0
                    if hist:
                        wait = max(wait, self.min_gap - (now - max(hist)))
                    recent = [t for t in hist if now - t < self.window]
                    if len(recent) >= self.max_per_window:
                        # Wait until the oldest one leaves the window.
                        wait = max(wait, self.window - (now - min(recent)))

                    if wait <= 0:
                        hist.append(now)
                        fh.seek(0)
                        fh.truncate()
                        fh.write(json.dumps([round(t, 3) for t in hist]))
                        fh.flush()
                        return waited
                finally:
                    _unlock(fh)

            if time.time() - started + wait > timeout:
                raise TimeoutError(
                    "%s gate: still throttled after %.0fs" % (self.label, timeout))
            time.sleep(min(wait, 5.0) + 0.01)
            waited += min(wait, 5.0)


class SingleInstance:
    """Refuse to run a second collector on this machine.

    The suspension happened with two collectors running at once. A rate gate
    alone would now hold the total in check, but two collectors also double the
    work for no benefit, so this makes the mistake impossible rather than
    merely survivable.
    """

    def __init__(self, name="collector"):
        os.makedirs(STATE_DIR, exist_ok=True)
        # The lock file is ONLY for locking -- the holder's identity lives in a
        # separate file. Writing into the byte range a lock covers is what made
        # the first version fail on Windows with "Permission denied".
        self.path = os.path.join(STATE_DIR, "%s.lock" % name)
        self.owner_path = os.path.join(STATE_DIR, "%s.owner" % name)
        self.fh = None

    def _read_owner(self):
        try:
            with open(self.owner_path, encoding="utf-8") as f:
                return f.read().strip() or "unknown"
        except OSError:
            return "unknown"

    def acquire(self):
        self.fh = open(self.path, "a+", encoding="utf-8")
        if not _lock(self.fh, blocking=False):
            other = self._read_owner()
            self.fh.close()
            self.fh = None
            return False, other
        try:
            with open(self.owner_path, "w", encoding="utf-8") as f:
                f.write("pid %d, started %s"
                        % (os.getpid(), time.strftime("%H:%M:%S")))
        except OSError:
            pass
        return True, None

    def release(self):
        if self.fh:
            _unlock(self.fh)
            self.fh.close()
            self.fh = None
            try:
                os.unlink(self.owner_path)
            except OSError:
                pass

    def __enter__(self):
        ok, other = self.acquire()
        if not ok:
            sys.exit("another instance is already running (%s).\n"
                     "Stop it first -- two collectors polling at once is what got "
                     "this machine's network suspended." % other)
        return self

    def __exit__(self, *a):
        self.release()


def stats():
    """Recent connection history, for auditing."""
    try:
        with open(GATE_FILE, encoding="utf-8") as fh:
            hist = [float(x) for x in json.loads(fh.read() or "[]")]
    except (OSError, ValueError, TypeError):
        return {"total_recorded": 0, "last_1s": 0, "last_10s": 0, "last_60s": 0}
    now = time.time()
    return {
        "total_recorded": len(hist),
        "last_1s": sum(1 for t in hist if now - t < 1),
        "last_10s": sum(1 for t in hist if now - t < 10),
        "last_60s": sum(1 for t in hist if now - t < 60),
        "peak_per_sec": _peak_per_sec(hist),
    }


def _peak_per_sec(hist):
    """Highest number of connections seen in any 1-second window."""
    hist = sorted(hist)
    peak = 0
    j = 0
    for i in range(len(hist)):
        while hist[i] - hist[j] > 1.0:
            j += 1
        peak = max(peak, i - j + 1)
    return peak


if __name__ == "__main__":
    g = ConnectionGate()
    print("gate file        : %s" % GATE_FILE)
    print("min gap          : %.2fs" % g.min_gap)
    print("window           : %d connections / %.0fs" % (g.max_per_window, g.window))
    print("sustained ceiling: %.2f conn/sec  (campus limit ~60/sec)" % g.rate_per_sec())
    print("hard ceiling     : %.2f conn/sec  (cannot be configured looser)"
          % HARD_CEILING_PER_SEC)
    print("history          : %s" % stats())
