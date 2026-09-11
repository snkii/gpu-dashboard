#!/usr/bin/env python3
# Copyright (c) 2026 Seonuk Kim, Human Interface Laboratory, Seoul National University
# SPDX-License-Identifier: MIT
"""Provision the sukim account + monitoring key across the HIL GPU servers.

One run walks every pending server. Each server has its own admin password,
so it prompts per server -- but it is a single continuous pass, and a failure
on one host does not stop the rest.

    pip install paramiko
    python provision/provision.py --dry-run
    python provision/provision.py
    python provision/provision.py --only hi14,hi15

Passwords are read with getpass (never echoed), held only in this process's
memory, dropped as soon as that server is finished, and never written to a
file, a command line, or the shell history. Nothing is stored, cached, or
derived -- and nothing here reads the lab spreadsheet.

What each server gets:
  * /usr/local/bin/hil-metrics   read-only nvidia-smi collector
  * user sukim                   password LOCKED (key-only), no sudo granted
  * ~sukim/.ssh/authorized_keys  login key + monitoring key pinned to
                                 command="/usr/local/bin/hil-metrics",restrict

Idempotent: re-running skips what is already in place.
"""
import argparse
import getpass
import json
import os
import posixpath
import socket
import sys

try:
    import paramiko
except ImportError:
    sys.exit("paramiko is required:  pip install paramiko")

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, ROOT)
import ratelimit                                      # noqa: E402

# Every outbound connection in this file goes through the machine-wide gate,
# including the cheap TCP preflight. Provisioning opens up to 3 connections per
# server (preflight, admin session, verify) and used to pace itself only by
# accident -- and an unpaced burst is what got this machine suspended.
GATE = ratelimit.ConnectionGate(label="provision")
SSH_DIR = os.path.join(os.path.expanduser("~"), ".ssh")
LOGIN_KEY = os.path.join(SSH_DIR, "hil_sukim.pub")
MONITOR_KEY = os.path.join(SSH_DIR, "hil_monitor.pub")

METRICS = """#!/bin/sh
# HIL GPU monitor: read-only. Installed by provision.py.
echo '###GPU'
nvidia-smi --query-gpu=index,name,uuid,utilization.gpu,memory.used,memory.total,power.draw,power.limit,temperature.gpu,fan.speed --format=csv,noheader,nounits 2>/dev/null
echo '###PROC'
nvidia-smi --query-compute-apps=gpu_uuid,pid,used_gpu_memory --format=csv,noheader,nounits 2>/dev/null
echo '###PS'
ps -eo pid=,user:24=,etimes=,comm= 2>/dev/null
echo '###LOAD'
cat /proc/loadavg 2>/dev/null
echo '###NPROC'
nproc 2>/dev/null
echo '###MEM'
free -m 2>/dev/null | grep -i '^Mem:'
echo '###DISK'
df -P -k / 2>/dev/null | tail -1
echo '###UP'
cut -d. -f1 /proc/uptime 2>/dev/null
echo '###END'
"""


def port_open(host, port, timeout=4):
    GATE.acquire()
    try:
        with socket.create_connection((host, port), timeout=timeout):
            return True
    except OSError:
        return False


def key_body(line):
    """The base64 blob, which is what identifies a key uniquely."""
    for tok in line.split():
        if tok.startswith("AAAA"):
            return tok
    return None


def run(client, cmd, password=None, timeout=60, extra_stdin=None):
    """Run one command. If it needs sudo, feed the password on stdin.

    sudo -S reads the password from stdin, so no tty is needed and the secret
    never appears in the command line (where any user could read it from ps).
    extra_stdin is written after the password, which is how data gets to a
    `tee` on the far side without going through shell quoting.
    """
    stdin, stdout, stderr = client.exec_command(cmd, timeout=timeout)
    if password is not None:
        stdin.write(password + "\n")
        stdin.flush()
    if extra_stdin is not None:
        stdin.write(extra_stdin)
        stdin.flush()
    stdin.channel.shutdown_write()
    out = stdout.read().decode("utf-8", "replace")
    err = stderr.read().decode("utf-8", "replace")
    rc = stdout.channel.recv_exit_status()
    return rc, out, err


def provision_one(srv, admin, password, login_pub, monitor_pub, dry_run=False):
    name, host, port = srv["name"], srv["host"], srv.get("port", 22)
    steps = []

    if dry_run:
        return "dry-run", ["would connect as %s@%s:%d" % (admin, host, port)]

    client = paramiko.SSHClient()
    client.load_system_host_keys()
    client.set_missing_host_key_policy(paramiko.AutoAddPolicy())
    GATE.acquire()
    try:
        client.connect(hostname=host, port=port, username=admin,
                       password=password, look_for_keys=False,
                       allow_agent=False, timeout=15, auth_timeout=15,
                       banner_timeout=15)
    except paramiko.AuthenticationException:
        # Do not hammer: these boxes may run fail2ban, and a few rejected
        # attempts can get this desktop's IP banned for hours -- which is how
        # hi14 dropped off the network mid-run.
        return "AUTH FAILED", [
            "password rejected for %s@%s" % (admin, host),
            "check the password before retrying: repeated failures can trigger"
            " a fail2ban block on this host",
        ]
    except (socket.timeout, socket.error, paramiko.SSHException) as e:
        return "UNREACHABLE", [str(e)]

    try:
        # --- 1. hil-metrics -------------------------------------------------
        sftp = client.open_sftp()
        tmp = "/tmp/hil-metrics.%d" % os.getpid()
        with sftp.file(tmp, "w") as f:
            f.write(METRICS)          # written with \n only; CRLF breaks sh
        sftp.close()
        rc, out, err = run(
            client,
            "sudo -S -p '' install -m 755 -o root -g root %s /usr/local/bin/hil-metrics "
            "&& rm -f %s" % (tmp, tmp),
            password=password)
        if rc != 0:
            return "FAILED", ["hil-metrics install: %s" % (err.strip() or rc)]
        steps.append("hil-metrics installed")

        # --- 2. account -----------------------------------------------------
        rc, out, _ = run(client, "id -u sukim >/dev/null 2>&1 && echo YES || echo NO")
        if out.strip() == "YES":
            steps.append("sukim already exists")
            # A pre-existing account can carry password aging. Once the
            # password is locked for key-only use, an expired one makes PAM
            # demand a change at login -- and with no tty that fails outright
            # ("Password change required but no TTY available"), locking the
            # account out even with a valid key. Clear the aging fields.
            # Single % here: this string is not %-formatted, and "%%Y" made
            # `date` emit a literal "%Y" that chage then rejected.
            # -d matters as much as -M/-E: an expired password has lastchg=0,
            # and only resetting that date stops PAM demanding a change.
            rc, out, err = run(
                client,
                "sudo -S -p '' chage -M -1 -E -1 -I -1 -d \"$(date +%Y-%m-%d)\" sukim",
                password=password)
            if rc == 0:
                steps.append("password aging cleared (was blocking key login)")
            else:
                steps.append("warn: chage failed: %s" % (err.strip()[:60] or rc))
        else:
            rc, out, err = run(
                client,
                "sudo -S -p '' useradd --create-home --shell /bin/bash sukim "
                "&& sudo -n passwd -l sukim >/dev/null 2>&1; echo rc=$?",
                password=password)
            rc2, out2, _ = run(client, "id -u sukim >/dev/null 2>&1 && echo YES || echo NO")
            if out2.strip() != "YES":
                return "FAILED", ["useradd: %s" % (err.strip() or out.strip())]
            steps.append("sukim created (password locked, no sudo)")

        # --- 3. keys --------------------------------------------------------
        rc, home, _ = run(client, "getent passwd sukim | cut -d: -f6")
        home = home.strip() or "/home/sukim"
        ak = posixpath.join(home, ".ssh", "authorized_keys")
        rc, out, err = run(
            client,
            "sudo -S -p '' sh -c 'install -d -m 700 -o sukim -g sukim %s && "
            "touch %s && chown sukim:sukim %s && chmod 600 %s'"
            % (posixpath.join(home, ".ssh"), ak, ak, ak),
            password=password)
        if rc != 0:
            return "FAILED", [".ssh setup: %s" % (err.strip() or rc)]

        # An earlier version of this script wrote the restricted line through
        # `sh -c "..."`, whose quoting stripped the double quotes and left
        #   command=/usr/local/bin/hil-metrics,restrict ssh-ed25519 ...
        # sshd rejects an unquoted command= option, so the line was dead -- and
        # a presence check on the key BODY still matches it, which would make
        # this run report "already present" and never repair it. Delete any
        # such line first.
        rc, out, err = run(
            client,
            "sudo -S -p '' sed -i '/^command=\\/usr\\/local\\/bin\\/hil-metrics,restrict/d' %s"
            % ak,
            password=password)
        if rc == 0 and out.strip() == "" :
            pass  # nothing to report; the line either was not there or is gone

        for label, line in (("login", login_pub),
                            ('monitor', 'command="/usr/local/bin/hil-metrics",restrict '
                                        + monitor_pub)):
            body = key_body(line)
            rc, out, _ = run(
                client,
                "sudo -S -p '' grep -qF '%s' %s && echo HAVE || echo MISSING" % (body, ak),
                password=password)
            if "HAVE" in out:
                steps.append("%s key already present" % label)
                continue
            # The line goes over stdin to `tee`, never through a shell string.
            # The restricted line contains double quotes --
            #   command="/usr/local/bin/hil-metrics",restrict ssh-ed25519 ...
            # -- which closed an enclosing sh -c "..." early and silently
            # stripped them, and sshd rejects an unquoted command= option, so
            # the whole key line was ignored and the key appeared not to work.
            rc, out, err = run(
                client, "sudo -S -p '' tee -a %s >/dev/null" % ak,
                password=password, extra_stdin=line + "\n")
            if rc != 0:
                return "FAILED", ["%s key append: %s" % (label, err.strip() or rc)]
            steps.append("%s key added" % label)

        # --- 4. verify ------------------------------------------------------
        rc, out, _ = run(client, "nvidia-smi --query-gpu=name --format=csv,noheader 2>/dev/null | wc -l")
        steps.append("%s GPUs visible" % out.strip())
        return "ok", steps
    finally:
        client.close()


def verify_monitor_key(srv, monitor_priv):
    """Confirm the monitoring key logs in AND cannot get a shell."""
    name, host, port = srv["name"], srv["host"], srv.get("port", 22)
    client = paramiko.SSHClient()
    client.set_missing_host_key_policy(paramiko.AutoAddPolicy())
    GATE.acquire()
    try:
        client.connect(hostname=host, port=port, username="sukim",
                       key_filename=monitor_priv, look_for_keys=False,
                       allow_agent=False, timeout=15)
        # Ask for something the forced command must refuse to run.
        _, stdout, _ = client.exec_command("cat /etc/shadow", timeout=30)
        out = stdout.read().decode("utf-8", "replace")
        if "###END" in out and "root:" not in out:
            gpu_block = out.split("###GPU", 1)[-1].split("###", 1)[0]
            n = len([l for l in gpu_block.splitlines() if l.strip()])
            return True, "forced command holds (%d GPUs)" % n
        return False, "forced command NOT enforced"
    except Exception as e:
        return False, str(e)[:60]
    finally:
        client.close()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("-c", "--config", default=os.path.join(ROOT, "servers.json"))
    # No default: the privileged account name is site-specific, and baking
    # one in would publish it. servers.json may set "admin_user".
    ap.add_argument("--admin", help="shared account with sudo (required, "
                                    "or set admin_user in servers.json)")
    ap.add_argument("--only", help="comma-separated server names")
    ap.add_argument("--force", action="store_true", help="redo already-provisioned servers")
    ap.add_argument("--dry-run", action="store_true")
    ap.add_argument("--verify-only", action="store_true",
                    help="skip provisioning; just test the monitoring key")
    args = ap.parse_args()

    with open(args.config, encoding="utf-8") as f:
        cfg = json.load(f)

    # The privileged account name is site-specific and is not published with
    # this code, so it comes from the (gitignored) inventory or the command line.
    if not args.admin:
        args.admin = cfg.get("admin_user")
    if not args.admin:
        sys.exit("no admin account: pass --admin NAME, or set \"admin_user\" "
                 "in %s" % args.config)

    for p in (LOGIN_KEY, MONITOR_KEY):
        if not os.path.isfile(p):
            sys.exit("missing %s -- generate it first:\n"
                     "  ssh-keygen -t ed25519 -f %s -C hil -N '\"\"'"
                     % (p, p[:-4]))
    login_pub = open(LOGIN_KEY, encoding="utf-8").read().strip()
    monitor_pub = open(MONITOR_KEY, encoding="utf-8").read().strip()
    monitor_priv = MONITOR_KEY[:-4]

    targets = [s for s in cfg["servers"] if s.get("enabled")]
    if args.only:
        want = {x.strip() for x in args.only.split(",")}
        targets = [s for s in targets if s["name"] in want]
    if args.verify_only:
        print("verifying monitoring key on %d server(s)\n" % len(targets))
        for s in targets:
            ok, msg = verify_monitor_key(s, monitor_priv)
            print("  %-5s %-4s %s" % (s["name"], "OK" if ok else "FAIL", msg))
        return 0
    if not args.force:
        targets = [s for s in targets if not s.get("provisioned")]

    if not targets:
        print("nothing to do (all provisioned; --force to redo)")
        return 0

    print("Targets (%d): %s" % (len(targets), ", ".join(s["name"] for s in targets)))
    print("rate gate     : %.2f conn/sec ceiling (campus limit ~60/sec)"
          % GATE.rate_per_sec())
    print("admin account : %s" % args.admin)
    print("grants sudo   : no")
    print("sets password : no (sukim is key-only, password locked)")

    if not args.dry_run:
        print("\nEach server has its own password, so you are asked once per")
        print("server. Nothing is saved, logged, or passed on a command line;")
        print("each one is dropped as soon as that server is done.")
        print("Press Enter on a prompt to skip that server.")

    results = []
    for s in targets:
        print("\n" + "=" * 60)
        print("%s  (%s:%s)" % (s["name"], s["host"], s.get("port", 22)))

        # Check reachability BEFORE asking for anything: prompting for a
        # password on a host that cannot be reached wastes the entry, and on a
        # host with fail2ban a doomed attempt costs part of the retry budget.
        if not args.dry_run and not port_open(s["host"], s.get("port", 22)):
            print("   - TCP %s:%s unreachable" % (s["host"], s.get("port", 22)))
            print("  -> UNREACHABLE (skipped, no password asked)")
            results.append((s, "UNREACHABLE"))
            continue

        password = None
        if not args.dry_run:
            password = getpass.getpass("  password for %s@%s: " % (args.admin, s["name"]))
            if not password:
                print("  -> skipped")
                results.append((s, "skipped"))
                continue

        try:
            status, steps = provision_one(s, args.admin, password,
                                          login_pub, monitor_pub, args.dry_run)
        finally:
            # Drop the secret as soon as this server is finished; nothing
            # carries over to the next iteration.
            password = None
        for st in steps:
            print("   - %s" % st)
        print("  -> %s" % status)
        results.append((s, status))

    print("\n" + "=" * 60)
    ok = [s for s, st in results if st == "ok"]
    for s, st in results:
        print("  %-5s %s" % (s["name"], st))

    if ok and not args.dry_run:
        print("\nverifying monitoring key is restricted to hil-metrics:")
        for s in ok:
            good, msg = verify_monitor_key(s, monitor_priv)
            print("  %-5s %-4s %s" % (s["name"], "OK" if good else "FAIL", msg))
        # Record success so a re-run skips them.
        names = {s["name"] for s in ok}
        for s in cfg["servers"]:
            if s["name"] in names:
                s["provisioned"] = True
        with open(args.config, "w", encoding="utf-8") as f:
            json.dump(cfg, f, ensure_ascii=False, indent=2)
            f.write("\n")
        print("\nservers.json updated (provisioned: true)")

    print("\nnext: python hilmon.py --check")
    return 0 if len(ok) == len(results) else 1


if __name__ == "__main__":
    sys.exit(main())
