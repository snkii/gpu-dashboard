#!/bin/sh
# Copyright (c) 2026 Seonuk Kim, Human Interface Laboratory, Seoul National University
# SPDX-License-Identifier: MIT
# Create the sukim account on one HIL GPU server, key-only.
#
# Run this ON the target server, as a user with sudo:
#     scp create-sukim.sh hi14:/tmp/ && ssh -t hi14 'sh /tmp/create-sukim.sh "<pubkey>"'
# or paste it into a root shell.
#
# Design notes:
#
#   * No password is ever set. The account is created with a LOCKED password
#     ("!" in /etc/shadow) and authenticates by SSH key only. Nothing here
#     reads, writes, or derives a password, so there is no plaintext secret to
#     leak into a log, a history file, or this script's arguments.
#
#   * Two keys are installed with different privileges:
#       - the interactive key gets a normal login
#       - the monitoring key is pinned to command="/usr/local/bin/hil-metrics"
#         with `restrict`, so even if it leaks it cannot open a shell
#     Pass only the interactive key to create the account; add the monitoring
#     key with --monitor-key.
#
#   * sudo is NOT granted. Each of these machines has its own listed
#     administrator; granting root to a new account is their decision, not
#     something a provisioning script should assume. Add it deliberately if a
#     server's owner agrees.
#
# Idempotent: safe to re-run. Existing keys are not duplicated.

set -eu

USER_NAME=sukim
LOGIN_KEY=""
MONITOR_KEY=""
SHELL_BIN=/bin/bash

usage() {
    cat >&2 <<EOF
usage: sh create-sukim.sh "<ssh-ed25519 AAAA... login key>" [--monitor-key "<key>"]

  --monitor-key KEY   also install a read-only key locked to hil-metrics
  --shell PATH        login shell (default: $SHELL_BIN)
EOF
    exit 2
}

[ $# -ge 1 ] || usage
LOGIN_KEY="$1"; shift
while [ $# -gt 0 ]; do
    case "$1" in
        --monitor-key) shift; [ $# -gt 0 ] || usage; MONITOR_KEY="$1" ;;
        --shell)       shift; [ $# -gt 0 ] || usage; SHELL_BIN="$1" ;;
        *) usage ;;
    esac
    shift
done

case "$LOGIN_KEY" in
    ssh-ed25519\ *|ssh-rsa\ *|ecdsa-sha2-*\ *) ;;
    *) echo "error: first argument does not look like an SSH public key" >&2; exit 2 ;;
esac

if [ "$(id -u)" -ne 0 ]; then
    SUDO="sudo"
    $SUDO -n true 2>/dev/null || { echo "error: need root or passwordless sudo" >&2; exit 1; }
else
    SUDO=""
fi

HOST=$(hostname -s 2>/dev/null || hostname)
echo "== $HOST =="

# --- account -----------------------------------------------------------------
if id "$USER_NAME" >/dev/null 2>&1; then
    echo "  user $USER_NAME: already exists (leaving as is)"
else
    [ -x "$SHELL_BIN" ] || SHELL_BIN=/bin/sh
    $SUDO useradd --create-home --shell "$SHELL_BIN" "$USER_NAME"
    # Locked password: key-only login, and `su - sukim` from root still works.
    $SUDO passwd -l "$USER_NAME" >/dev/null 2>&1 || true
    echo "  user $USER_NAME: created (shell $SHELL_BIN, password locked)"
fi

HOME_DIR=$(getent passwd "$USER_NAME" | cut -d: -f6)
[ -n "$HOME_DIR" ] || { echo "  error: no home directory for $USER_NAME" >&2; exit 1; }

# --- ssh keys ----------------------------------------------------------------
AK="$HOME_DIR/.ssh/authorized_keys"
$SUDO install -d -m 700 -o "$USER_NAME" -g "$USER_NAME" "$HOME_DIR/.ssh"
$SUDO touch "$AK"
$SUDO chown "$USER_NAME:$USER_NAME" "$AK"
$SUDO chmod 600 "$AK"

add_key() {
    # $1 = full authorized_keys line, $2 = label
    # Match on the base64 body, which always begins with AAAA. Field 2 is
    # wrong: a restricted line reads
    #   command="...",restrict ssh-ed25519 AAAA... comment
    # so field 2 is the key TYPE, and grepping for "ssh-ed25519" matches any
    # existing key of that type -- which silently skipped installing the
    # monitoring key whenever a login key was already present.
    key_body=$(printf '%s' "$1" | tr ' ' '\n' | grep '^AAAA' | head -1)
    if [ -z "$key_body" ]; then
        echo "  key $2: SKIPPED (could not parse key body)" >&2
        return 1
    fi
    if $SUDO grep -qF "$key_body" "$AK" 2>/dev/null; then
        echo "  key $2: already present"
    else
        printf '%s\n' "$1" | $SUDO tee -a "$AK" >/dev/null
        echo "  key $2: added"
    fi
}

add_key "$LOGIN_KEY" "login"

if [ -n "$MONITOR_KEY" ]; then
    if [ ! -x /usr/local/bin/hil-metrics ]; then
        echo "  warn: /usr/local/bin/hil-metrics missing -- install it first" >&2
        echo "        (see deploy/DEPLOY-hilgpu.com.md step 1)" >&2
    fi
    add_key "command=\"/usr/local/bin/hil-metrics\",restrict $MONITOR_KEY" "monitor(read-only)"
fi

# --- report ------------------------------------------------------------------
echo "  groups : $(id -nG "$USER_NAME" 2>/dev/null || echo '?')"
if command -v nvidia-smi >/dev/null 2>&1; then
    echo "  gpus   : $(nvidia-smi --query-gpu=name --format=csv,noheader 2>/dev/null | wc -l) visible"
else
    echo "  gpus   : nvidia-smi not found"
fi
echo "  sudo   : not granted (add deliberately if this server's owner agrees)"
echo "  done"
