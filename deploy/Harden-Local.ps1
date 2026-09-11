# Copyright (c) 2026 Seonuk Kim, Human Interface Laboratory, Seoul National University
# SPDX-License-Identifier: MIT
<#
    Tighten the local files that hold credentials.

    Everything here is already user-writable only by default. What this removes
    is the inherited Administrators and SYSTEM entries on the SSH private keys:
    nothing on this machine needs to read them but the account that uses them,
    and a key readable by any process running as SYSTEM is a key one local
    privilege escalation away from being copied.

    OpenSSH on Windows is stricter than the default ACL, not laxer -- it
    refuses a private key whose ACL grants access beyond the owner, so this
    also removes a class of "Permissions are too open" failure.

    Safe to re-run. Reports what it changed and verifies afterwards.
#>
[CmdletBinding()]
param([switch] $WhatIf)

$ErrorActionPreference = 'Stop'

$targets = @(
    "$env:USERPROFILE\.ssh\hil_monitor"
    "$env:USERPROFILE\.ssh\hil_sukim"
    "$env:USERPROFILE\.hilgpu\gemini.key"
    "$env:APPDATA\rclone\rclone.conf"
)

$me = "$env:USERDOMAIN\$env:USERNAME"

foreach ($f in $targets) {
    if (-not (Test-Path $f)) { Write-Host "skip (absent): $f"; continue }

    $before = (Get-Acl $f).Access |
        Where-Object { $_.AccessControlType -eq 'Allow' } |
        ForEach-Object { $_.IdentityReference.Value } |
        Sort-Object -Unique

    if ($before.Count -eq 1 -and $before[0] -eq $me) {
        Write-Host ("already owner-only: {0}" -f $f)
        continue
    }

    Write-Host ("tightening {0}" -f $f)
    Write-Host ("  from: {0}" -f ($before -join ', '))

    if ($WhatIf) { Write-Host "  (WhatIf: not changed)"; continue }

    # icacls, not Set-Acl. Set-Acl writes the whole security descriptor --
    # owner and SACL included -- so Windows demands SeSecurityPrivilege even
    # when only the DACL changed and you already own the file. A standard
    # account does not hold that privilege. icacls touches the DACL alone and
    # the owner is allowed to do that.
    # /inheritance:r alone is not enough: on these files Administrators and
    # SYSTEM are EXPLICIT entries, not inherited ones, so they survive it and
    # icacls still reports success. They have to be named.
    & icacls $f /inheritance:r `
        /remove "BUILTIN\Administrators" "NT AUTHORITY\SYSTEM" "BUILTIN\Users" `
        /grant:r "${me}:(F)" | Out-Null
    if ($LASTEXITCODE -ne 0) { Write-Warning "  icacls exited $LASTEXITCODE"; continue }

    $after = (Get-Acl $f).Access |
        Where-Object { $_.AccessControlType -eq 'Allow' } |
        ForEach-Object { $_.IdentityReference.Value } |
        Sort-Object -Unique
    Write-Host ("  to  : {0}" -f ($after -join ', '))
    if ($after.Count -ne 1) { Write-Warning "  more than one principal still has access" }
}

Write-Host "`nverifying the monitoring key still works (one connection, rate-gated)..."
$probe = & ssh -i "$env:USERPROFILE\.ssh\hil_monitor" -o IdentitiesOnly=yes -o BatchMode=yes `
              -o ConnectTimeout=10 -o StrictHostKeyChecking=accept-new `
              sukim@147.46.126.88 "true" 2>&1 | Select-Object -First 3
if ($probe -match '###GPU') {
    Write-Host "OK - the forced command still answers, so the key is still readable by us." -ForegroundColor Green
} else {
    Write-Host "unexpected response:" -ForegroundColor Yellow
    $probe
}
