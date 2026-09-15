# Copyright (c) 2026 Seonuk Kim, Human Interface Laboratory, Seoul National University
# SPDX-License-Identifier: MIT
<#
    Put an R2 access key pair into the rclone remote the collector uploads with.

    R2 credentials are NOT the same thing as the Cloudflare API token in
    ~/.hilgpu/cloudflare.token. The API token changes zone settings; this pair
    is an S3 key that writes objects into the bucket. They are issued in
    different places and revoking one does not touch the other -- but "delete
    all my API tokens" does take out both, and when that happened the page
    froze within seconds while the collector went on working perfectly, because
    the only thing that broke was the last hop.

    Make the pair at
        Cloudflare dashboard -> R2 -> API -> Manage API tokens -> Create
    with:
        Permissions   Object Read & Write
        Buckets       this bucket only, not "All buckets"
    and take the Access Key ID and Secret Access Key it shows once.

    Usage:
        .\deploy\Set-R2Credentials.ps1
        .\deploy\Set-R2Credentials.ps1 -Verify     # test what is stored
#>
[CmdletBinding()]
param(
    [switch] $Verify
)

$ErrorActionPreference = 'Stop'

$S      = & (Join-Path $PSScriptRoot 'load-settings.ps1')
$Remote = $S.Remote
$Bucket = $S.Bucket
$Rclone = Join-Path $S.Root 'bin\rclone.exe'

if (-not (Test-Path $Rclone)) { throw "missing: $Rclone" }

$conf = (& $Rclone config file | Select-Object -Last 1).Trim()
if (-not (Test-Path $conf)) { throw "no rclone config at $conf" }

function Test-Remote {
    # lsd on the bucket, not on the remote: a key scoped to one bucket cannot
    # ListBuckets, and calling that would report a working key as broken.
    #
    # cmd does the stderr merge, not PowerShell. `2>&1` on a native command in
    # 5.1 wraps each stderr line in an ErrorRecord, and with
    # ErrorActionPreference = Stop that turns the 401 message we are trying to
    # read into a thrown NativeCommandError instead.
    $text = & cmd /c "`"$Rclone`" lsd `"${Remote}:${Bucket}`" --retries 1 2>&1" | Out-String
    [pscustomobject]@{ Ok = ($LASTEXITCODE -eq 0); Text = $text.Trim() }
}

if (-not $Verify) {
    Write-Host "Access Key ID is not secret; the secret is masked and not echoed.`n"
    $id = (Read-Host "R2 Access Key ID").Trim()

    $secure = Read-Host "R2 Secret Access Key" -AsSecureString
    $bstr   = [Runtime.InteropServices.Marshal]::SecureStringToBSTR($secure)
    try { $secret = [Runtime.InteropServices.Marshal]::PtrToStringBSTR($bstr) }
    finally { [Runtime.InteropServices.Marshal]::ZeroFreeBSTR($bstr) }
    $secret = $secret.Trim()

    foreach ($pair in @(@('Access Key ID', $id), @('Secret Access Key', $secret))) {
        $name, $val = $pair
        if (-not $val)              { throw "$name is empty" }
        if ($val -match '\s')       { throw "$name contains whitespace -- it did not paste cleanly" }
        if ($val -match '[^\x21-\x7E]') { throw "$name contains non-printable characters" }
        if ($val.Length -lt 20)     { throw "$name is only $($val.Length) characters -- that looks truncated" }
    }

    # .NET file IO, not Get-Content/Set-Content. On this machine Get-Content
    # decodes UTF-8 as the ANSI code page and Set-Content writes a BOM; either
    # one silently corrupts a config file. Read as UTF-8 and write back with
    # whatever BOM the file already had.
    $bytes  = [IO.File]::ReadAllBytes($conf)
    $hadBom = $bytes.Length -ge 3 -and $bytes[0] -eq 0xEF -and $bytes[1] -eq 0xBB -and $bytes[2] -eq 0xBF
    $text   = [Text.Encoding]::UTF8.GetString($bytes)
    if ($hadBom) { $text = $text.Substring(1) }

    # Only inside this remote's section -- the file may hold others.
    $lines = $text -split "`r?`n"
    $inSection = $false
    $seen = @{}
    for ($i = 0; $i -lt $lines.Count; $i++) {
        $line = $lines[$i]
        if ($line -match '^\s*\[(.+)\]\s*$') {
            $inSection = ($matches[1] -eq $Remote)
            continue
        }
        if (-not $inSection) { continue }
        if ($line -match '^\s*access_key_id\s*=') {
            $lines[$i] = "access_key_id = $id";          $seen['id'] = $true
        } elseif ($line -match '^\s*secret_access_key\s*=') {
            $lines[$i] = "secret_access_key = $secret";  $seen['secret'] = $true
        }
    }
    if (-not ($seen['id'] -and $seen['secret'])) {
        throw "did not find both keys inside [$Remote] in $conf -- not writing a half-updated config"
    }

    $enc = New-Object Text.UTF8Encoding($hadBom)
    [IO.File]::WriteAllText($conf, ($lines -join "`r`n"), $enc)
    $secret = $null

    # The config now holds a live key in plain text, so let only this account
    # read it. Same reasoning as the API token file.
    $me = "$env:USERDOMAIN\$env:USERNAME"
    & icacls $conf /inheritance:r `
        /remove "BUILTIN\Administrators" "NT AUTHORITY\SYSTEM" "BUILTIN\Users" `
        /grant:r "${me}:(F)" | Out-Null

    Write-Host "`nwrote: $conf" -ForegroundColor Green
}

$who = (Get-Acl $conf).Access |
    Where-Object { $_.AccessControlType -eq 'Allow' } |
    ForEach-Object { $_.IdentityReference.Value } | Sort-Object -Unique
Write-Host "readable by: $($who -join ', ')"

Write-Host "`nverifying against ${Remote}:${Bucket} ..."
$r = Test-Remote
if (-not $r.Ok) {
    Write-Host "REJECTED:" -ForegroundColor Red
    Write-Host $r.Text
    exit 1
}
Write-Host "OK. The collector's next tick will upload." -ForegroundColor Green
