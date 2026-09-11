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
# Beside the project, not under AppData. A scheduled task launched by this
# account could not open a log in %LOCALAPPDATA%\gpu-dashboard for append --
# it exited 1 before running anything, with nothing written anywhere to say
# why -- while the identical command worked when run by hand. The collector
# already writes its power log into this directory from the same task, so this
# path is known to work.
$LogDir = Join-Path $Root "logs"
$Log    = Join-Path $LogDir "watchdog.log"
$Task   = "HILGPUWatchdog"

# One call every ten minutes is 144 a day, comfortably inside the free tier's
# daily cap while still being fresh enough for a sentence about a cluster.
$SummaryInterval = 600
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
    "up-summary.cmd" = "copyto summary.json $Dest/summary.json"
}
foreach ($name in $scripts.Keys) {
    $body = "@echo off`r`n`"$Rclone`" $($scripts[$name])`r`n"
    Set-Content -Path (Join-Path $Bin $name) -Value $body -Encoding ascii -NoNewline
}

# The collector's command line lives in its own .cmd file too.
#
# `cmd /c "<long quoted command>"` only strips its outer quote pair under
# conditions that are easy to fall out of. When it does not, the trailing quote
# is left dangling, the redirect fails to parse, and cmd exits 1 having written
# nothing at all -- no log, no process, no clue. That is precisely what adding
# two more flags caused. A file has no outer quoting to get wrong.
#
# The summary needs no flag of its own to be safe: the collector looks for the
# API key file and stays quiet when it is absent.
$runner = Join-Path $Bin "run-collector.cmd"
$runnerBody = @"
@echo off
rem Written by deploy/install-daemon.ps1. Edit that, not this.
"$Exe" --publish "$Out" --loop ^
  --upload-cmd "$Bin\up-first.cmd" ^
  --upload-cmd-tick "$Bin\up-status.cmd" ^
  --upload-cmd-stats "$Bin\up-stats.cmd" --stats-interval 60 ^
  --upload-cmd-summary "$Bin\up-summary.cmd" --summary-interval $SummaryInterval ^
  >> "$Log" 2>&1
"@
Set-Content -Path $runner -Value $runnerBody -Encoding ascii

$arguments = "/c `"$runner`""

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

