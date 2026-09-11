# Copyright (c) 2026 Seonuk Kim, Human Interface Laboratory, Seoul National University
# SPDX-License-Identifier: MIT
<#
    Keep the HIL GPU collector running across reboots, power cuts and crashes.

    There are two paths to the same collector, because the original task
    (HILGPUMonitor) was registered by an elevated process and its ACL now
    refuses every non-admin change -- it cannot be repointed or deleted without
    an admin prompt:

      1. HILGPUMonitor   - existing, S4U, at boot. Runs hilmon.py, which
                           delegates straight to bin\hilmon.exe. This is the
                           path that works before anyone logs in.
      2. HILGPUWatchdog  - registered here, Interactive (all a standard user
                           may register), at logon and every minute after.
                           Runs bin\hilmon.exe directly, so the monitor keeps
                           working even if Python is upgraded or removed.

    Running both is safe: the collector takes a cross-process lock and a second
    instance exits immediately, because two collectors would split one SSH rate
    budget -- which is what got this machine's network suspended once already.

    Safe to re-run.
#>

$ErrorActionPreference = "Stop"

# Paths and the storage remote name identify one machine and one account, so
# they come from local.settings.ps1, which is gitignored.
$S      = & (Join-Path $PSScriptRoot 'load-settings.ps1')
$Root   = $S.Root
$Bin    = Join-Path $Root "bin"
$Exe    = Join-Path $Bin  "hilmon.exe"
$Rclone = Join-Path $Bin  "rclone.exe"
$Out    = Join-Path $Root "out"
$LogDir = Join-Path $env:LOCALAPPDATA "gpu-dashboard"
$Log    = Join-Path $LogDir "watchdog.log"
$Task   = "HILGPUWatchdog"
$Dest   = "$($S.Remote):$($S.Bucket)"

foreach ($p in @($Exe, $Rclone)) {
    if (-not (Test-Path $p)) { throw "missing: $p" }
}
New-Item -ItemType Directory -Force -Path $LogDir, $Out | Out-Null

# The upload commands live in their own .cmd files rather than inline in the
# task's argument string. Inline, each one has to survive being quoted for the
# task, unquoted by cmd, re-parsed by the collector and handed to another cmd;
# the backslash-quote escaping that requires is fragile enough that it silently
# produced "'\"...rclone.exe\"' is not recognized" instead of an upload. A file
# path with no spaces needs no quoting at any layer.
#
# One-way push: the desktop sends bytes out to R2 and nothing on the public
# side can reach back in. rclone is the bundled copy, not winget's
# version-stamped path, which an upgrade would rename out from under the task.
$scripts = @{
    "up-first.cmd"  = "copy . $Dest --checksum --no-traverse"
    "up-status.cmd" = "copyto status.json $Dest/status.json"
    "up-stats.cmd"  = "copyto stats.json $Dest/stats.json"
}
foreach ($name in $scripts.Keys) {
    $body = "@echo off`r`n`"$Rclone`" $($scripts[$name])`r`n"
    Set-Content -Path (Join-Path $Bin $name) -Value $body -Encoding ascii -NoNewline
}

$inner = "`"$Exe`" --publish `"$Out`" --loop" +
         " --upload-cmd $Bin\up-first.cmd" +
         " --upload-cmd-tick $Bin\up-status.cmd" +
         " --upload-cmd-stats $Bin\up-stats.cmd" +
         " --stats-interval 60" +
         " >> `"$Log`" 2>&1"
# cmd /c strips the first and last quote of its argument, so the whole command
# gets one extra pair for cmd to eat. cmd is here only for the >> redirection.
$arguments = "/c `"$inner`""

if (Get-ScheduledTask -TaskName $Task -ErrorAction SilentlyContinue) {
    # Unregister alone leaves a running instance holding the collector lock,
    # and the replacement then finds the lock taken and does nothing.
    Stop-ScheduledTask -TaskName $Task -ErrorAction SilentlyContinue
    Start-Sleep -Seconds 3
    Unregister-ScheduledTask -TaskName $Task -Confirm:$false
}

$action = New-ScheduledTaskAction -Execute "$env:SystemRoot\System32\cmd.exe" `
                                  -Argument $arguments -WorkingDirectory $Root

# At logon, then a poll every minute. When the collector is already up the
# poll costs one process that exits in milliseconds; when it is not, this is
# what brings it back.
$tLogon = New-ScheduledTaskTrigger -AtLogOn -User $env:USERNAME
$tLogon.Delay = "PT30S"                       # let the network come up first
$tPoll = New-ScheduledTaskTrigger -Once -At (Get-Date).Date.AddMinutes(1) `
             -RepetitionInterval (New-TimeSpan -Minutes 1)
try   { $tPoll.Repetition.Duration = $null }  # $null means "indefinitely"
catch { $tPoll.Repetition.Duration = "P3650D" }

# Interactive is the only logon type a standard user may register; S4U needs
# admin. The trade-off is that this trigger waits for a logon, which is exactly
# why HILGPUMonitor's boot trigger is still worth keeping.
$principal = New-ScheduledTaskPrincipal -UserId $env:USERNAME `
                                        -LogonType Interactive -RunLevel Limited

$settings = New-ScheduledTaskSettingsSet `
    -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries `
    -StartWhenAvailable -RestartCount 999 -RestartInterval (New-TimeSpan -Minutes 1) `
    -MultipleInstances IgnoreNew -ExecutionTimeLimit (New-TimeSpan -Seconds 0)
$settings.DisallowStartOnRemoteAppSession = $false
$settings.RunOnlyIfNetworkAvailable = $false   # we retry ourselves, never skip

Register-ScheduledTask -TaskName $Task -Action $action `
    -Trigger @($tLogon, $tPoll) -Principal $principal -Settings $settings `
    -Description "HIL GPU monitor watchdog: starts the collector if it is not running" |
    Out-Null

Write-Output "registered $Task"

# Free the lock, then let the watchdog take it.
Get-Process hilmon -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Seconds 2
Start-ScheduledTask -TaskName $Task
Start-Sleep -Seconds 25

foreach ($t in @("HILGPUMonitor", $Task)) {
    Get-ScheduledTaskInfo -TaskName $t | Format-List TaskName, LastRunTime, LastTaskResult
}
Write-Output "--- tail of $Log ---"
Get-Content $Log -Tail 6

