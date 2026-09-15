# Copyright (c) 2026 Seonuk Kim, Human Interface Laboratory, Seoul National University
# SPDX-License-Identifier: MIT
<#
    Store a narrowly scoped Cloudflare API token so zone settings can be
    changed without a person at the keyboard.

    Why a file and not a prompt: the deployment scripts read their token from a
    masked prompt, which is the safest thing when a person is running them. It
    also means nothing else can run them -- an assistant working in a
    non-interactive shell cannot answer a prompt. Putting the token in a file
    that only this account can read trades a little exposure for the ability to
    apply and verify a setting in one step.

    That trade is only acceptable because the token is scoped. Make it at
        https://dash.cloudflare.com/profile/api-tokens
    with "Create Custom Token" and NOTHING more than:

        Zone / Zone Settings / Edit      changes TLS version, Always Use HTTPS
        Zone / Transform Rules / Edit    deploys the security headers
        Zone / WAF / Edit                the rule that limits the site to campus
        Zone / Zone / Read               looks the zone up by name

    and under "Zone Resources" pick the single zone, not "All zones".

    A token like that cannot read R2, cannot touch DNS, cannot see billing, and
    cannot reach any other domain. If it leaks, the worst case is someone
    editing headers on one site, or taking the campus restriction off it.

    A token with every permission does not belong in a file. It can move money,
    read every zone, and mint more credentials; the blast radius of the file
    leaking stops being "one site" and becomes "the whole account".

    Usage:
        .\deploy\Set-CloudflareToken.ps1
        .\deploy\Set-CloudflareToken.ps1 -Verify     # test what is stored
        .\deploy\Set-CloudflareToken.ps1 -Remove
#>
[CmdletBinding()]
param(
    [switch] $Remove,
    [switch] $Verify
)

$ErrorActionPreference = 'Stop'

$dir  = Join-Path $env:USERPROFILE '.hilgpu'
$path = Join-Path $dir 'cloudflare.token'
$api  = 'https://api.cloudflare.com/client/v4'

# Only for the account id, and only to verify. Missing settings are not fatal:
# storing a token has to keep working on a machine that has not been set up yet.
$S = $null
try { $S = & (Join-Path $PSScriptRoot 'load-settings.ps1') } catch { }

if ($Remove) {
    if (Test-Path $path) {
        Remove-Item $path -Force
        Write-Host "removed $path" -ForegroundColor Yellow
        Write-Host "Revoke it at https://dash.cloudflare.com/profile/api-tokens as well --"
        Write-Host "deleting the file does not invalidate the token."
    } else {
        Write-Host "nothing stored."
    }
    return
}

New-Item -ItemType Directory -Force -Path $dir | Out-Null

if (-not $Verify) {
    Write-Host "Paste the Cloudflare API token. Input is masked and is not echoed."
    Write-Host "  Scope it to ONE zone with Zone Settings:Edit + Transform Rules:Edit"
    Write-Host "  + WAF:Edit (+ Zone:Read). Anything wider does not belong in a file.`n"
    $secure = Read-Host "Cloudflare API token" -AsSecureString
    $bstr   = [Runtime.InteropServices.Marshal]::SecureStringToBSTR($secure)
    try { $tok = [Runtime.InteropServices.Marshal]::PtrToStringBSTR($bstr) }
    finally { [Runtime.InteropServices.Marshal]::ZeroFreeBSTR($bstr) }

    if (-not $tok) { throw 'no token entered' }
    $tok = $tok.Trim()
    # Only catch a paste that went wrong; the API decides whether it is valid.
    if ($tok -match '\s') { throw 'the token contains a space or newline -- it did not paste cleanly' }
    if ($tok -match '[^\x21-\x7E]') { throw 'the token contains non-printable characters' }
    if ($tok.Length -lt 20) { throw "only $($tok.Length) characters -- that looks truncated" }

    Set-Content -Path $path -Value $tok -Encoding ascii -NoNewline

    # icacls, not Set-Acl: Set-Acl writes the whole security descriptor and
    # Windows then demands SeSecurityPrivilege, which a standard account does
    # not hold. And name the principals -- inherited entries are not the only
    # ones present.
    $me = "$env:USERDOMAIN\$env:USERNAME"
    & icacls $path /inheritance:r `
        /remove "BUILTIN\Administrators" "NT AUTHORITY\SYSTEM" "BUILTIN\Users" `
        /grant:r "${me}:(F)" | Out-Null

    Write-Host "`nstored: $path" -ForegroundColor Green
    Write-Host "length: $($tok.Length) characters (the token itself is not shown)"
    $tok = $null
}

if (-not (Test-Path $path)) { throw "no token stored at $path" }

$who = (Get-Acl $path).Access |
    Where-Object { $_.AccessControlType -eq 'Allow' } |
    ForEach-Object { $_.IdentityReference.Value } | Sort-Object -Unique
Write-Host "readable by: $($who -join ', ')"
if ($who.Count -ne 1) { Write-Warning "more than one principal can read the token" }

$stored = (Get-Content $path -Raw).Trim()
$hdr = @{ Authorization = "Bearer $stored"; 'Content-Type' = 'application/json' }
$stored = $null

Write-Host "`nverifying with Cloudflare..."

# Two kinds of token, two verify endpoints. A token made under the user profile
# answers at /user/tokens/verify; one made under an account answers only at
# /accounts/<id>/tokens/verify and returns a flat 401 "Invalid API Token" at the
# user endpoint -- which reads exactly like a bad paste and is not. So try both
# before calling it rejected.
$v = $null
foreach ($uri in @("$api/user/tokens/verify", "$api/accounts/$($S.AccountId)/tokens/verify")) {
    if ($uri -match '/accounts//') { continue }      # no account id configured
    try {
        $r = Invoke-RestMethod -Uri $uri -Headers $hdr -TimeoutSec 25
        if ($r.success) { $v = $r; break }
    } catch { }
}
if (-not $v) {
    Write-Host "REJECTED: neither the user nor the account endpoint accepted it." -ForegroundColor Red
    Write-Host "Check that the whole token pasted, and that it has not been rolled."
    exit 1
}
Write-Host "token status : $($v.result.status)" -ForegroundColor Green

# Show what it can actually reach, so an over-broad token is obvious now
# rather than after it leaks.
try {
    $zones = Invoke-RestMethod -Uri "$api/zones" -Headers $hdr -TimeoutSec 25
    $names = $zones.result | ForEach-Object { $_.name }
    Write-Host "zones visible: $(if ($names) { $names -join ', ' } else { '(none listed -- Zone:Read not granted, which is fine)' })"
    if ($names.Count -gt 1) {
        Write-Warning "this token can see $($names.Count) zones. Scope it to one and re-run."
    }
} catch {
    Write-Host "zones visible: (cannot list -- Zone:Read not granted, which is fine)"
}

Write-Host "`nOK. Zone settings can now be applied without a prompt." -ForegroundColor Green
