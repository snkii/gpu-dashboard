# Copyright (c) 2026 Seonuk Kim, Human Interface Laboratory, Seoul National University
# SPDX-License-Identifier: MIT
<#
    Write the rclone config for the hilgpu R2 bucket.

    You paste the two R2 credentials at a masked prompt. They go straight from
    the prompt into the config file: never echoed, never in a command line,
    never in PSReadLine history, and never through a chat window. That last one
    matters -- a secret pasted into a transcript has to be treated as leaked,
    which is why this script exists.

    Usage:
        .\deploy\Set-RcloneConfig.ps1
        .\deploy\Set-RcloneConfig.ps1 -Verify      # test the connection after
#>
[CmdletBinding()]
param(
    [string] $Remote,
    [string] $Bucket,
    [string] $AccountId,
    [switch] $Verify
)

# Identifiers that must not be published live in local.settings.ps1, which is
# gitignored. Anything passed explicitly on the command line wins over it.
$S = & (Join-Path $PSScriptRoot 'load-settings.ps1')

if (-not $Remote)    { $Remote    = $S.Remote }
if (-not $Bucket)    { $Bucket    = $S.Bucket }
if (-not $AccountId) { $AccountId = $S.AccountId }

$ErrorActionPreference = 'Stop'

$dir  = Join-Path $env:APPDATA 'rclone'
$path = Join-Path $dir 'rclone.conf'
New-Item -ItemType Directory -Force -Path $dir | Out-Null

Write-Host "rclone config : $path"
Write-Host "R2 endpoint   : https://$AccountId.r2.cloudflarestorage.com"
Write-Host "bucket        : $Bucket`n"

if (Test-Path $path) {
    $existing = Get-Content $path -Raw -Encoding UTF8
    if ($existing -match "(?m)^\[$([regex]::Escape($Remote))\]") {
        Write-Host "A [$Remote] section already exists; it will be replaced." -ForegroundColor Yellow
        $bak = "$path.bak-$(Get-Date -Format yyyyMMddHHmmss)"
        Copy-Item $path $bak
        Write-Host "backup: $bak"
    }
} else {
    $existing = ''
}

Write-Host "`nPaste the values from the R2 token screen."
Write-Host "Input is masked - nothing is displayed or logged.`n"

$keyId  = Read-Host "Access Key ID"     -AsSecureString
$secret = Read-Host "Secret Access Key" -AsSecureString

function Reveal([System.Security.SecureString] $s) {
    $b = [Runtime.InteropServices.Marshal]::SecureStringToBSTR($s)
    try { [Runtime.InteropServices.Marshal]::PtrToStringBSTR($b) }
    finally { [Runtime.InteropServices.Marshal]::ZeroFreeBSTR($b) }
}

$k = Reveal $keyId
$s = Reveal $secret
if (-not $k -or -not $s) { throw "both values are required" }

# Drop any [hilgpu] block that is already there, keeping other remotes intact.
$kept = @()
$skip = $false
foreach ($line in ($existing -split "`r?`n")) {
    if ($line -match '^\[(.+)\]\s*$') { $skip = ($Matches[1] -eq $Remote) }
    if (-not $skip) { $kept += $line }
}

$block = @"
[$Remote]
type = s3
provider = Cloudflare
access_key_id = $k
secret_access_key = $s
endpoint = https://$AccountId.r2.cloudflarestorage.com
region = auto
acl = private
no_check_bucket = true
"@

$out = (($kept -join "`n").TrimEnd() + "`n`n" + $block).TrimStart()
Set-Content -Path $path -Value $out -Encoding utf8 -NoNewline:$false

# Scrub the plaintext copies from this process as soon as they are on disk.
$k = $null; $s = $null; $block = $null; $out = $null
[GC]::Collect()

# The file holds a live secret: make it readable only by this account.
try {
    $acl = Get-Acl $path
    $acl.SetAccessRuleProtection($true, $false)   # drop inherited permissions
    $acl.Access | ForEach-Object { [void]$acl.RemoveAccessRule($_) }
    $acl.AddAccessRule((New-Object System.Security.AccessControl.FileSystemAccessRule(
        "$env:USERDOMAIN\$env:USERNAME", 'FullControl', 'Allow')))
    Set-Acl -Path $path -AclObject $acl
    Write-Host "`npermissions   : $env:USERNAME only (inheritance removed)"
} catch {
    Write-Warning "could not tighten file permissions: $($_.Exception.Message)"
}

Write-Host "written       : [$Remote] section in $path"

if ($Verify) {
    Write-Host "`nverifying..."
    # List INSIDE the bucket, not the account's buckets. A token scoped to one
    # bucket has no ListBuckets permission, so "rclone lsd hilgpu:" returns 403
    # AccessDenied even when the credentials are perfectly good -- that 403 is
    # the scoping working, not a failure.
    & rclone size "${Remote}:${Bucket}" 2>&1 | Select-Object -First 3
    if ($LASTEXITCODE -eq 0) {
        Write-Host "connection OK - bucket '$Bucket' is reachable" -ForegroundColor Green
    } else {
        Write-Host "connection FAILED (exit $LASTEXITCODE)" -ForegroundColor Yellow
        Write-Host "  403 AccessDenied on ListBuckets is expected and harmless;"
        Write-Host "  anything else means the key or secret did not paste cleanly."
    }
}

Write-Host "`nnext:"
Write-Host "  cd `$env:USERPROFILE\Desktop\hilmon"
Write-Host "  rclone copy out ${Remote}:${Bucket} --checksum --progress"
