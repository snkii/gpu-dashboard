# Copyright (c) 2026 Seonuk Kim, Human Interface Laboratory, Seoul National University
# SPDX-License-Identifier: MIT
<#
    Provision the sukim account + monitoring key across the HIL GPU servers.

    Reads servers.json, skips the ones already working, and for each remaining
    server: copies create-sukim.sh, runs it under an admin account, then adds a
    matching ~/.ssh/config entry so `ssh hi25` just works afterwards.

    You will be prompted for the admin account's password once per server.
    That prompt comes from ssh itself -- the password is never passed as an
    argument, written to a file, or echoed, so it stays out of the process list
    and out of PSReadLine history.

    Usage:
        .\provision\Provision-Servers.ps1 -WhatIf          # show the plan only
        .\provision\Provision-Servers.ps1                  # all pending servers
        .\provision\Provision-Servers.ps1 -Only hi25,hi27  # just these
        .\provision\Provision-Servers.ps1 -AdminUser hil
#>
[CmdletBinding(SupportsShouldProcess = $true)]
param(
    # Shared account used to create sukim. Needs sudo on the target.
    [string]   $AdminUser = 'hil',
    [string[]] $Only,
    # Re-run even for servers already marked provisioned.
    [switch]   $Force,
    [switch]   $SkipConfig
)

$ErrorActionPreference = 'Stop'

$root       = Split-Path -Parent $PSScriptRoot
$configJson = Join-Path $root 'servers.json'
$shScript   = Join-Path $PSScriptRoot 'create-sukim.sh'
$sshDir     = Join-Path $env:USERPROFILE '.ssh'
$sshConfig  = Join-Path $sshDir 'config'
$loginKey   = Join-Path $sshDir 'hil_sukim'
$monitorKey = Join-Path $sshDir 'hil_monitor'

foreach ($f in @($configJson, $shScript)) {
    if (-not (Test-Path $f)) { throw "missing: $f" }
}

# ---------- keys ----------
# ssh-keygen on Windows will not take an empty -N "" from PowerShell (the arg
# is swallowed and it errors with "option requires an argument"). '""' is the
# documented workaround.
function New-KeyIfMissing {
    param([string] $Path, [string] $Comment)
    if (Test-Path $Path) {
        Write-Host "  key exists : $(Split-Path -Leaf $Path)"
        return
    }
    if ($WhatIfPreference) {
        Write-Host "  would generate : $(Split-Path -Leaf $Path)"
        return
    }
    Write-Host "  generating : $(Split-Path -Leaf $Path)"
    & ssh-keygen -t ed25519 -f $Path -C $Comment -N '""' -q
    if (-not (Test-Path $Path)) { throw "ssh-keygen failed for $Path" }
}

if (-not (Test-Path $sshDir)) { New-Item -ItemType Directory -Path $sshDir | Out-Null }
Write-Host "SSH keys"
New-KeyIfMissing -Path $loginKey   -Comment 'sukim login'
New-KeyIfMissing -Path $monitorKey -Comment 'hilmon readonly'

if (-not $WhatIfPreference) {
    $loginPub   = (Get-Content "$loginKey.pub"   -Raw).Trim()
    $monitorPub = (Get-Content "$monitorKey.pub" -Raw).Trim()
}

# ---------- targets ----------
# -Encoding UTF8 is required: servers.json holds Korean rack labels, and
# Windows PowerShell's Get-Content defaults to the ANSI codepage, which mangles
# them badly enough to break the JSON parse.
$cfg     = Get-Content $configJson -Raw -Encoding UTF8 | ConvertFrom-Json
$servers = $cfg.servers | Where-Object { $_.enabled }
if ($Only)   { $servers = $servers | Where-Object { $Only -contains $_.name } }
if (-not $Force) { $servers = $servers | Where-Object { -not $_.provisioned } }

if (-not $servers) { Write-Host "`nnothing to do (all provisioned; use -Force to redo)"; return }

Write-Host "`nTargets ($($servers.Count)):"
$servers | ForEach-Object { Write-Host ("  {0,-6} {1}:{2}" -f $_.name, $_.host, $_.port) }
Write-Host "`nadmin account : $AdminUser  (password prompt per server)"
Write-Host "grants sudo   : no"
Write-Host "sets password : no (sukim is key-only, password locked)`n"

# The remote read-only helper the monitoring key is pinned to.
$metrics = @'
#!/bin/sh
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
'@ -replace "`r`n", "`n"

# Written once to a temp file and scp'd to each host. LF endings and no BOM:
# a CRLF or BOM in a shebang script makes the remote shell fail with
# "bad interpreter" or "^M: not found".
$metricsTmp = Join-Path $env:TEMP 'hil-metrics'
if (-not $WhatIfPreference) {
    [System.IO.File]::WriteAllText($metricsTmp, $metrics, (New-Object System.Text.UTF8Encoding($false)))
}

$results = @()

foreach ($s in $servers) {
    $name = $s.name; $target = "$AdminUser@$($s.host)"; $port = $s.port
    Write-Host ("=" * 62)
    Write-Host "$name  ($($s.host):$port)"

    if (-not $PSCmdlet.ShouldProcess($name, 'create sukim + install keys')) {
        $results += [pscustomobject]@{ Server = $name; Result = 'skipped (WhatIf)' }
        continue
    }

    try {
        # 1. Upload both scripts as files rather than piping them into the
        #    remote shell. stdin has to stay attached to the terminal: sudo
        #    needs a tty to prompt, and a piped stdin yields
        #    "sudo: no tty present and no askpass program specified".
        & scp -P $port -o StrictHostKeyChecking=accept-new $shScript "${target}:/tmp/create-sukim.sh"
        if ($LASTEXITCODE -ne 0) { throw "scp create-sukim.sh failed (exit $LASTEXITCODE)" }
        & scp -P $port -o StrictHostKeyChecking=accept-new $metricsTmp "${target}:/tmp/hil-metrics"
        if ($LASTEXITCODE -ne 0) { throw "scp hil-metrics failed (exit $LASTEXITCODE)" }

        # 2. Run with -t so sudo can prompt. Single quotes around the keys are
        #    safe: they are base64 tokens containing no quote characters.
        $remote = "set -e; " +
                  "sudo install -m 755 /tmp/hil-metrics /usr/local/bin/hil-metrics; " +
                  "sudo sh /tmp/create-sukim.sh '$loginPub' --monitor-key '$monitorPub'; " +
                  "rm -f /tmp/create-sukim.sh /tmp/hil-metrics"

        & ssh -p $port -o StrictHostKeyChecking=accept-new -t $target $remote
        if ($LASTEXITCODE -ne 0) { throw "remote script failed (exit $LASTEXITCODE)" }

        $results += [pscustomobject]@{ Server = $name; Result = 'ok' }
        Write-Host "  -> ok" -ForegroundColor Green
    }
    catch {
        $results += [pscustomobject]@{ Server = $name; Result = "FAILED: $($_.Exception.Message)" }
        Write-Host "  -> $($_.Exception.Message)" -ForegroundColor Yellow
    }
}

# ---------- ~/.ssh/config ----------
if (-not $SkipConfig -and -not $WhatIfPreference) {
    $ok = $results | Where-Object { $_.Result -eq 'ok' }
    if ($ok) {
        Copy-Item $sshConfig "$sshConfig.bak-$(Get-Date -Format yyyyMMddHHmmss)" -ErrorAction SilentlyContinue
        $existing = ''
        if (Test-Path $sshConfig) { $existing = Get-Content $sshConfig -Raw }
        $add = New-Object System.Text.StringBuilder
        foreach ($r in $ok) {
            $s = $servers | Where-Object { $_.name -eq $r.Server } | Select-Object -First 1
            if ($existing -match "(?m)^\s*Host\s+.*\b$($s.name)\b") {
                Write-Host "config: $($s.name) already present"
                continue
            }
            [void]$add.AppendLine("")
            [void]$add.AppendLine("Host $($s.name)")
            [void]$add.AppendLine("    HostName $($s.host)")
            [void]$add.AppendLine("    Port $($s.port)")
            [void]$add.AppendLine("    User sukim")
            [void]$add.AppendLine("    IdentityFile ~/.ssh/hil_monitor")
            [void]$add.AppendLine("    IdentitiesOnly yes")
            [void]$add.AppendLine("    ServerAliveInterval 30")
        }
        if ($add.Length -gt 0) {
            Add-Content -Path $sshConfig -Value $add.ToString() -Encoding utf8
            Write-Host "config: appended entries to $sshConfig (backup made)"
        }
    }
}

Write-Host "`n$('=' * 62)"
$results | Format-Table -AutoSize
Write-Host "next: python hilmon.py --check"
