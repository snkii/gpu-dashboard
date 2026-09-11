# Copyright (c) 2026 Seonuk Kim, Human Interface Laboratory, Seoul National University
# SPDX-License-Identifier: MIT
<#
    Register the HIL GPU collector as a Windows scheduled task.

    Survives an unexpected shutdown: the trigger is AtStartup (not logon), so a
    power cut followed by a reboot brings the collector back on its own, and the
    restart policy revives it if the process dies. The public site stays up
    regardless -- it is static files on Cloudflare -- so the worst case of a
    desktop outage is a stale-data banner, never a dead page.

    Usage (normal PowerShell, no admin needed for a per-user task):
        .\deploy\install-task-windows.ps1
        .\deploy\install-task-windows.ps1 -Remote mybucket:mybucket
        .\deploy\install-task-windows.ps1 -Remove
#>
[CmdletBinding()]
param(
    # rclone destination. Leave empty to publish locally without uploading.
    [string] $Remote,
    [string] $TaskName = 'HILGPUMonitor',
    [switch] $Interactive,
    [switch] $Remove
)

$ErrorActionPreference = 'Stop'

$root    = Split-Path -Parent $PSScriptRoot
$script  = Join-Path $root 'hilmon.py'
$outDir  = Join-Path $root 'out'
# Identifiers that must not be published live in local.settings.ps1.
if (-not $Remote) {
    $S = & (Join-Path $PSScriptRoot 'load-settings.ps1')
    $Remote = "$($S.Remote):$($S.Bucket)"
}
$logDir  = Join-Path $env:LOCALAPPDATA 'gpu-dashboard'
$logFile = Join-Path $logDir 'publish.log'

if ($Remove) {
    if (Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue) {
        Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false
        Write-Host "removed scheduled task '$TaskName'"
    } else {
        Write-Host "no such task: '$TaskName'"
    }
    return
}

if (-not (Test-Path $script)) { throw "hilmon.py not found at $script" }

# Resolve a real python.exe: the WindowsApps alias shim does not work under a
# scheduled task that runs without a logon session.
$python = $null
foreach ($c in @('python.exe', 'python3.exe', 'py.exe')) {
    $cmd = Get-Command $c -ErrorAction SilentlyContinue
    if ($cmd -and $cmd.Source -and $cmd.Source -notlike '*WindowsApps*') {
        $python = $cmd.Source; break
    }
}
if (-not $python) {
    throw 'No usable python.exe on PATH. Install Python from python.org (not the Store alias).'
}
Write-Host "python     : $python"

New-Item -ItemType Directory -Force -Path $logDir  | Out-Null
New-Item -ItemType Directory -Force -Path $outDir  | Out-Null

# NOT $args -- that is a PowerShell automatic variable, and writing to it at
# script scope is asking for a confusing collision later.
$cliArgs = @('--publish', "`"$outDir`"", '--loop')
if ($Remote) {
    # Look it up against the PATH as PERSISTED, not as this shell happens to
    # have it: a shell opened before the install has a stale PATH, and winget
    # drops rclone in a versioned Packages directory that is only ever on the
    # USER PATH -- which a scheduled task does not necessarily inherit.
    $rcPath = $null
    $r = Get-Command rclone.exe -ErrorAction SilentlyContinue
    if ($r) { $rcPath = $r.Source }
    if (-not $rcPath) {
        $persisted = ([Environment]::GetEnvironmentVariable('Path', 'Machine') + ';' +
                      [Environment]::GetEnvironmentVariable('Path', 'User')) -split ';'
        foreach ($dir in $persisted) {
            if ($dir -and (Test-Path (Join-Path $dir 'rclone.exe'))) {
                $rcPath = (Join-Path $dir 'rclone.exe'); break
            }
        }
    }
    if (-not $rcPath) {
        $pkgs = "$env:LOCALAPPDATA\Microsoft\WinGet\Packages"
        if (Test-Path $pkgs) {
            $hit = Get-ChildItem $pkgs -Recurse -Filter rclone.exe -Depth 3 `
                -ErrorAction SilentlyContinue | Select-Object -First 1
            if ($hit) { $rcPath = $hit.FullName }
        }
    }
    $rclone = $rcPath
    if (-not $rclone) {
        Write-Warning 'rclone.exe not found; installing the task without upload. Re-run after: winget install Rclone.Rclone'
    } else {
        # Runs inside $outDir after each publish. --checksum avoids re-uploading
        # the unchanged page and icons every tick; only status.json changes.
        # First tick uploads everything; every tick after that uploads the one
        # file that actually changed. `copyto` addresses a single object, so it
        # costs one operation instead of eleven.
        $cliArgs += @('--upload-cmd',
                      "`"\`"$rcPath\`" copy . $Remote --checksum --no-traverse`"")
        $cliArgs += @('--upload-cmd-tick',
                      "`"\`"$rcPath\`" copyto status.json $Remote/status.json`"")
        Write-Host "upload     : $rcPath"
        Write-Host "             first=copy all, then=copyto status.json only"
    }
}

# Wrap in cmd.exe purely to append stdout+stderr to one rolling log.
#
# The whole command line gets ONE MORE pair of quotes around it. When the text
# after /c starts with a quote, cmd strips the first and the last quote of the
# line -- which here tore the quotes off the python path and off the log path,
# so the task exited 1 and never even created the log file. The extra outer
# pair is the documented workaround: cmd /c "<the real command line>".
# -u: unbuffered. Redirected to a file, Python buffers stdout, so the log
# stays EMPTY for a long time even while the collector is working fine --
# which makes a healthy task look dead.
$inner   = "`"$python`" -u `"$script`" " + ($cliArgs -join ' ')
$cmdLine = '/c "' + $inner + " >> `"$logFile`" 2>&1" + '"'
$action  = New-ScheduledTaskAction -Execute "$env:SystemRoot\System32\cmd.exe" `
    -Argument $cmdLine -WorkingDirectory $root

# Two triggers, because they cover different failures:
#   AtStartup  - survives a reboot or a power cut, with no logon needed.
#   Repetition - a keep-alive. RestartCount only fires when the process exits
#                with a FAILURE; a clean exit, a kill, or an exhausted restart
#                budget would otherwise leave the collector down for good.
#                MultipleInstances=IgnoreNew makes the re-trigger a no-op while
#                it is already running, so this can never start a second one.
$tStartup = New-ScheduledTaskTrigger -AtStartup
$tStartup.Delay = 'PT30S'          # let the network come up first

$tKeepAlive = New-ScheduledTaskTrigger -Once -At (Get-Date).AddMinutes(1) `
    -RepetitionInterval (New-TimeSpan -Minutes 10)

$settings = New-ScheduledTaskSettingsSet `
    -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries `
    -StartWhenAvailable `
    -RestartInterval (New-TimeSpan -Minutes 1) -RestartCount 3 `
    -ExecutionTimeLimit (New-TimeSpan -Seconds 0) `
    -MultipleInstances IgnoreNew `
    -DontStopOnIdleEnd

# Run as the current user so ~/.ssh keys and rclone.conf are visible.
#
# S4U means "run whether the user is logged on or not" -- a real daemon that
# comes back after a reboot with nobody signed in. Registering an S4U task
# needs an elevated shell; without it Windows returns a bare
# "Access denied" (0x80070005), which says nothing about why.
# Interactive is the fallback: no admin needed, but the task only runs while
# this user is signed in.
$isAdmin = ([Security.Principal.WindowsPrincipal] `
    [Security.Principal.WindowsIdentity]::GetCurrent()
).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)

if ($Interactive -or -not $isAdmin) {
    if (-not $Interactive) {
        Write-Host ""
        Write-Warning "Not running as Administrator."
        Write-Host "  A true daemon (runs with nobody logged on) needs an elevated shell."
        Write-Host "  Right-click PowerShell -> 'Run as administrator', then:"
        Write-Host "      cd `"$root`""
        Write-Host "      Set-ExecutionPolicy -Scope Process Bypass -Force"
        Write-Host "      .\deploy\install-task-windows.ps1"
        Write-Host ""
        Write-Host "  Falling back to logon-only mode: the collector will run whenever"
        Write-Host "  you are signed in, and start automatically at sign-in after a reboot."
        Write-Host ""
    }
    $principal = New-ScheduledTaskPrincipal -UserId "$env:USERDOMAIN\$env:USERNAME" `
        -LogonType Interactive -RunLevel Limited
    # AtStartup never fires for an Interactive task; trigger at logon instead.
    $tStartup = New-ScheduledTaskTrigger -AtLogOn -User "$env:USERDOMAIN\$env:USERNAME"
    $tStartup.Delay = 'PT30S'
    $mode = "logon-only (not elevated)"
} else {
    $principal = New-ScheduledTaskPrincipal -UserId "$env:USERDOMAIN\$env:USERNAME" `
        -LogonType S4U -RunLevel Limited
    $mode = "daemon (runs without logon, survives reboot)"
}

if (Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue) {
    try {
        # Stop it FIRST. Unregister-ScheduledTask does not kill a running
        # instance, so the old collector keeps holding the single-instance
        # lock and the freshly registered task exits 1 the moment it starts.
        Stop-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
        Start-Sleep -Milliseconds 800
        Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false
    } catch {
        # A task registered from an elevated shell cannot be replaced from a
        # normal one. Without this check the next line fails with the far less
        # helpful "file already exists".
        Write-Host ""
        Write-Host "Cannot replace the existing '$TaskName' task from this shell." -ForegroundColor Yellow
        Write-Host "It was registered with administrator rights, so removing or"
        Write-Host "replacing it needs the same. Open PowerShell as administrator:"
        Write-Host ""
        Write-Host "    cd `"$root`""
        Write-Host "    Set-ExecutionPolicy -Scope Process Bypass -Force"
        Write-Host "    .\deploy\install-task-windows.ps1"
        Write-Host ""
        exit 1
    }
}
Register-ScheduledTask -TaskName $TaskName -Action $action `
    -Trigger @($tStartup, $tKeepAlive) `
    -Settings $settings -Principal $principal `
    -Description 'HIL GPU monitor: polls hi* servers and publishes a redacted snapshot.' | Out-Null

Start-ScheduledTask -TaskName $TaskName
Start-Sleep -Seconds 6

$info  = Get-ScheduledTask -TaskName $TaskName | Get-ScheduledTaskInfo
$state = (Get-ScheduledTask -TaskName $TaskName).State
Write-Host ""
Write-Host "task       : $TaskName"
Write-Host "mode       : $mode"
Write-Host "state      : $state  (last result: $($info.LastTaskResult))"
Write-Host "log        : $logFile"
Write-Host "output dir : $outDir"
Write-Host ""
Write-Host "checks:"
Write-Host "  Get-Content `"$logFile`" -Tail 20 -Wait"
Write-Host "  Get-ScheduledTask $TaskName | Get-ScheduledTaskInfo"
Write-Host "  .\deploy\install-task-windows.ps1 -Remove     # to uninstall"
