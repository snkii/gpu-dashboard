#!/usr/bin/env python3
# Copyright (c) 2026 Seonuk Kim, Human Interface Laboratory, Seoul National University
# SPDX-License-Identifier: MIT
"""Install the streaming collector on each server. No password needed.

Everything this touches -- ~/.hil-stream and ~/.ssh/authorized_keys -- is owned
by sukim, so the existing login key is enough. sudo is never used.

    python provision/install-stream.py --dry-run
    python provision/install-stream.py
    python provision/install-stream.py --only hi14,hi15
    python provision/install-stream.py --revert     # back to one-shot polling

What changes on the server:
  * ~/.hil-stream is written (mode 700)
  * the monitoring key's authorized_keys line is repointed:
        command="/home/sukim/.hil-metrics",restrict  ->  command=".../.hil-stream",restrict
    The key itself, and every other key in the file, is left alone.

Reverting just points the line back; the old one-shot script stays in place.
"""
import argparse
import io
import os
import subprocess
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, ROOT)
import ratelimit                                       # noqa: E402
import hilmon                                          # noqa: E402

GATE = ratelimit.ConnectionGate(label="install-stream")
SSH_DIR = os.path.join(os.path.expanduser("~"), ".ssh")
LOGIN_KEY = os.path.join(SSH_DIR, "hil_sukim")
MONITOR_PUB = os.path.join(SSH_DIR, "hil_monitor.pub")
STREAM_SH = os.path.join(ROOT, "provision", "hil-stream.sh")


def key_body(pub):
    for tok in pub.split():
        if tok.startswith("AAAA"):
            return tok
    return None


def ssh(host, port, script, timeout=60):
    """Run a shell script on the server over the LOGIN key.

    The script is piped in as BYTES: a text-mode pipe on Windows rewrites \\n
    as \\r\\n, and a CR inside a heredoc makes the remote shell fail with
    "Illegal option -".
    """
    GATE.acquire()
    argv = ["ssh", "-p", str(port), "-i", LOGIN_KEY,
            "-o", "IdentitiesOnly=yes", "-o", "BatchMode=yes",
            "-o", "ConnectTimeout=10", "-o", "StrictHostKeyChecking=accept-new",
            "sukim@" + host, "sh -s"]
    r = subprocess.run(argv, input=script.encode("utf-8"),
                       capture_output=True, timeout=timeout)
    return (r.returncode,
            r.stdout.decode("utf-8", "replace"),
            r.stderr.decode("utf-8", "replace"))


def build_script(stream_body, mon_pub, revert):
    """Remote script: write ~/.hil-stream, then repoint the key's command=."""
    target = "$HOME/.hil-metrics" if revert else "$HOME/.hil-stream"
    body = key_body(mon_pub)
    return (
        'set -e\n'
        'H=$HOME\n'
        'AK="$H/.ssh/authorized_keys"\n'
        + ('' if revert else
           'cat > "$H/.hil-stream" <<\'HILSTREAM_EOF\'\n'
           + stream_body.rstrip("\n") + '\nHILSTREAM_EOF\n'
           'chmod 700 "$H/.hil-stream"\n')
        + 'if [ ! -f "$AK" ]; then echo "NO_AUTHORIZED_KEYS"; exit 1; fi\n'
          'cp "$AK" "$AK.bak.$(date +%s)"\n'
          # Rewrite only the line carrying the monitoring key body. Any other
          # key in the file -- a teammate's, the login key -- is untouched.
          'awk -v body="' + body + '" -v cmd="' + target + '" \'\n'
          '  index($0, body) > 0 { print "command=\\"" cmd "\\",restrict " $2 " " $3; next }\n'
          '  { print }\n'
          '\' "$AK" > "$AK.new"\n'
          'if [ ! -s "$AK.new" ]; then echo "REWRITE_EMPTY"; rm -f "$AK.new"; exit 1; fi\n'
          'mv "$AK.new" "$AK"\n'
          'chmod 600 "$AK"\n'
          'echo "keys=$(wc -l < "$AK") restricted=$(grep -c \'^command=\' "$AK")"\n'
          'grep -o \'command="[^"]*"\' "$AK" | head -1\n'
    )


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("-c", "--config", default=os.path.join(ROOT, "servers.json"))
    ap.add_argument("--only")
    ap.add_argument("--revert", action="store_true")
    ap.add_argument("--dry-run", action="store_true")
    args = ap.parse_args()

    cfg = hilmon.load_config(args.config)
    targets = [s for s in cfg["servers"] if s.get("enabled")]
    if args.only:
        want = {x.strip() for x in args.only.split(",")}
        targets = [s for s in targets if s["name"] in want]
    if not targets:
        sys.exit("no matching servers")

    for p in (LOGIN_KEY, MONITOR_PUB, STREAM_SH):
        if not os.path.isfile(p):
            sys.exit("missing: %s" % p)

    stream_body = io.open(STREAM_SH, encoding="utf-8").read().replace("\r\n", "\n")
    mon_pub = io.open(MONITOR_PUB, encoding="utf-8").read().strip()
    script = build_script(stream_body, mon_pub, args.revert)

    print("%s on %d server(s): %s" % (
        "REVERTING" if args.revert else "installing stream",
        len(targets), ", ".join(s["name"] for s in targets)))
    print("auth: login key (no password, no sudo)")
    print("rate: %.2f conn/sec ceiling\n" % GATE.rate_per_sec())

    if args.dry_run:
        print("--- remote script ---")
        print(script)
        return 0

    ok = 0
    for s in targets:
        rc, out, err = ssh(s["host"], s.get("port", 22), script)
        if rc == 0:
            ok += 1
            print("  %-5s OK   %s" % (s["name"], " | ".join(out.split())))
        else:
            print("  %-5s FAIL rc=%d %s" % (s["name"], rc, (err or out).strip()[:70]))

    print("\n%d/%d done" % (ok, len(targets)))
    if ok and not args.revert:
        print("next: python hilmon.py --stream-check")
    return 0 if ok == len(targets) else 1


if __name__ == "__main__":
    sys.exit(main())
