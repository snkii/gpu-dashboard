#!/bin/sh
# Copyright (c) 2026 Seonuk Kim, Human Interface Laboratory, Seoul National University
# SPDX-License-Identifier: MIT
# HIL GPU monitor - streaming collector, installed as ~/.hil-stream.
#
# Pinned as the forced command of the monitoring key, so this is the ONLY thing
# that key can run. It never exits on its own: one SSH connection is opened and
# the data keeps flowing down it.
#
# Why streaming instead of one connection per sample: the campus network flags
# a host that opens many SSH connections per second. Data on an ALREADY OPEN
# connection is not a new connection, so this gives 1-second numbers while
# opening fewer connections than the old 32-second polling did -- one per
# server, ever, instead of 13 every 32 seconds.
#
# Output is one block per tick:
#   ###GPU     always
#   ###PROC .. ###UP   only every SLOW ticks (they change slowly and cost more)
#   ###END     terminates the block
#
# The reader merges each fast block onto the last slow one.

INTERVAL=${HILMON_INTERVAL:-1}
SLOW=${HILMON_SLOW:-20}

# Never let a stuck nvidia-smi wedge the stream. `timeout` is in coreutils on
# every one of these boxes; if it is missing, run the query bare.
if command -v timeout >/dev/null 2>&1; then
    TO="timeout 5"
else
    TO=""
fi

GPU_Q='index,name,uuid,utilization.gpu,memory.used,memory.total,power.draw,power.limit,temperature.gpu,fan.speed'

i=0
while :; do
    echo '###GPU'
    $TO nvidia-smi --query-gpu="$GPU_Q" --format=csv,noheader,nounits 2>/dev/null

    if [ $((i % SLOW)) -eq 0 ]; then
        echo '###PROC'
        $TO nvidia-smi --query-compute-apps=gpu_uuid,pid,used_gpu_memory \
            --format=csv,noheader,nounits 2>/dev/null
        # No ###PS. The old one-shot script shipped the whole process table --
        # thousands of lines on a busy box -- and the dashboard only ever used
        # the job COUNT per GPU, which ###PROC already gives. Owner names are
        # stripped before publishing anyway, so sending them was pure cost.
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
    fi

    echo '###END'
    i=$((i + 1))
    sleep "$INTERVAL"
done
