# Copyright (c) 2026 Seonuk Kim, Human Interface Laboratory, Seoul National University
# SPDX-License-Identifier: MIT
<#
    Create (or replace) the security-header Transform Rule on hilgpu.com, and
    turn on Always Use HTTPS -- in one call instead of seven form fields.

    You paste a Cloudflare API token at a masked prompt. It is used for this
    run only: never echoed, never written to disk, never on a command line.
    Nothing here stores it, so the next run asks again.

    Make the token at
        https://dash.cloudflare.com/profile/api-tokens
    with "Create Custom Token", scoped to "Specific zone -> hilgpu.com":

        Zone / Transform Rules / Edit    REQUIRED - deploys the header rule
        Zone / Zone Settings / Edit      optional - flips Always Use HTTPS
        Zone / Zone / Read               optional - looks the zone up by name;
                                         skip it and pass -ZoneId instead

    Only the first is needed. The script reports and continues when the
    optional ones are missing, so the narrowest useful token is one
    permission. It cannot read R2, touch DNS, or reach another zone.

    Usage:
        .\deploy\Set-SecurityHeaders.ps1
        .\deploy\Set-SecurityHeaders.ps1 -WhatIf     # show what would be sent
#>
[CmdletBinding(SupportsShouldProcess = $true)]
param(
    [string] $Zone,
    [string] $ZoneId,
    [string] $AccountId,
    [string] $RuleName  = 'security headers',
    # Leave empty to keep whatever is already configured. Only set this if you
    # want the script to change it.
    [ValidateSet('', '1.0', '1.1', '1.2', '1.3')]
    [string] $MinTls    = ''
)

# Identifiers that must not be published live in local.settings.ps1, which is
# gitignored. Anything passed explicitly on the command line wins over it.
$S = & (Join-Path $PSScriptRoot 'load-settings.ps1')
if (-not $Zone)      { $Zone      = $S.Zone }
if (-not $AccountId) { $AccountId = $S.AccountId }

$ErrorActionPreference = 'Stop'

# Ordered so the output reads the same way every run.
$headers = [ordered]@{
    'Strict-Transport-Security'    = 'max-age=63072000; includeSubDomains'
    'X-Content-Type-Options'       = 'nosniff'
    'X-Frame-Options'              = 'DENY'
    'Referrer-Policy'              = 'no-referrer'
    'Cross-Origin-Opener-Policy'   = 'same-origin'
    'Cross-Origin-Resource-Policy' = 'same-origin'
    'Permissions-Policy'           = 'geolocation=(), camera=(), microphone=()'
    # The page has a robots meta tag; status.json and the rest cannot have one,
    # because there is nowhere in a JSON file to put it. A header covers every
    # response whatever its type.
    'X-Robots-Tag'                 = 'noindex, nofollow, noarchive'
    'Content-Security-Policy'      = @(
        "default-src 'none'"
        "script-src 'unsafe-inline'"
        "style-src 'unsafe-inline'"
        "connect-src 'self'"
        "img-src 'self' data:"
        "manifest-src 'self'"
        "base-uri 'none'"
        "form-action 'none'"
        "frame-ancestors 'none'"
    ) -join '; '
}

Write-Host "zone    : $Zone"
Write-Host "rule    : $RuleName"
Write-Host "headers : $($headers.Count)"
$headers.GetEnumerator() | ForEach-Object {
    $v = $_.Value
    if ($v.Length -gt 64) { $v = $v.Substring(0, 61) + '...' }
    Write-Host ("          {0,-30} {1}" -f $_.Key, $v)
}

if ($WhatIfPreference) { Write-Host "`n(WhatIf: nothing sent)"; return }

# A stored token, if one was set up with Set-CloudflareToken.ps1. Without it
# the token is asked for here and kept only for this run -- which is safer, but
# also means nothing can run this script unattended.
$tokenFile = Join-Path $env:USERPROFILE '.hilgpu\cloudflare.token'
if (Test-Path $tokenFile) {
    $token = (Get-Content $tokenFile -Raw).Trim()
    Write-Host "`nusing the stored token ($tokenFile)"
} else {
    Write-Host "`nPaste the API token. Input is masked and is not saved anywhere."
    $secure = Read-Host "Cloudflare API token" -AsSecureString
    $bstr   = [Runtime.InteropServices.Marshal]::SecureStringToBSTR($secure)
    try { $token = [Runtime.InteropServices.Marshal]::PtrToStringBSTR($bstr) }
    finally { [Runtime.InteropServices.Marshal]::ZeroFreeBSTR($bstr) }
}
if (-not $token) { throw 'no token entered' }

$auth = @{ Authorization = "Bearer $token"; 'Content-Type' = 'application/json' }
$api  = 'https://api.cloudflare.com/client/v4'

function Invoke-CF {
    param([string] $Method, [string] $Path, $Body)
    $args = @{ Method = $Method; Uri = "$api$Path"; Headers = $auth }
    if ($Body) { $args.Body = ($Body | ConvertTo-Json -Depth 12 -Compress) }
    try {
        $r = Invoke-RestMethod @args
    } catch {
        $msg = $_.Exception.Message
        if ($_.ErrorDetails.Message) { $msg = $_.ErrorDetails.Message }
        throw "$Method $Path failed: $msg"
    }
    if (-not $r.success) {
        throw "$Method $Path returned errors: $($r.errors | ConvertTo-Json -Compress)"
    }
    return $r
}

# --- find the zone ---------------------------------------------------------
# Looking a zone up BY NAME needs Zone:Read, which is a different permission
# from the two this script actually uses. Rather than widen the token, take
# the id directly when it is given -- both Edit permissions work fine without
# ever listing zones.
$zoneId = $ZoneId
if (-not $zoneId) {
    try {
        $z = Invoke-CF GET "/zones?name=$Zone"
        if ($z.result -and $z.result.Count -gt 0) { $zoneId = $z.result[0].id }
    } catch {
        Write-Host "zone lookup failed (this needs Zone:Read)" -ForegroundColor Yellow
    }
}
if (-not $zoneId) {
    Write-Host ""
    Write-Host "Could not resolve the zone id from the token." -ForegroundColor Yellow
    Write-Host "Listing zones by name needs a 'Zone / Zone / Read' permission that"
    Write-Host "this script does not otherwise require. Two ways forward:"
    Write-Host ""
    Write-Host "  A. Pass the id (no token change). Find it on the domain Overview"
    Write-Host "     page, right-hand column, under 'API' -> 'Zone ID':"
    Write-Host "       https://dash.cloudflare.com/$AccountId/$Zone"
    Write-Host "     then:"
    Write-Host "       .\deploy\Set-SecurityHeaders.ps1 -ZoneId <32-hex-id>"
    Write-Host ""
    Write-Host "  B. Add 'Zone / Zone / Read' to the token and re-run."
    Write-Host ""
    throw "zone id required"
}
Write-Host "`nzone id : $zoneId"

# --- Always Use HTTPS ------------------------------------------------------
# This is the setting that fixes the browser's "not secure" warning: without
# it a bare hostname is served over plain HTTP and never redirected.
#
# Non-fatal: it needs Zone Settings:Edit, which the header rule does not, and
# it is a one-time toggle that is just as easily flipped in the dashboard.
# Failing the whole run over it would block the part that actually needs a
# script.
try {
    Invoke-CF PATCH "/zones/$zoneId/settings/always_use_https" @{ value = 'on' } | Out-Null
    Write-Host "always_use_https : on"
} catch {
    if ($_.Exception.Message -match 'Authentication error') {
        Write-Host "always_use_https : SKIPPED (token lacks Zone Settings:Edit)" -ForegroundColor Yellow
        Write-Host "                   flip it here if it is not already on:"
        Write-Host "                   https://dash.cloudflare.com/$AccountId/$Zone/ssl-tls/edge-certificates"
    } else {
        Write-Warning "always_use_https not set: $($_.Exception.Message)"
    }
}

if ($MinTls) {
    # Only touched when asked. Silently forcing 1.2 would DOWNGRADE a zone
    # already set to 1.3, which is stricter.
    try {
        Invoke-CF PATCH "/zones/$zoneId/settings/min_tls_version" @{ value = $MinTls } | Out-Null
        Write-Host "min_tls_version  : $MinTls"
    } catch {
        Write-Warning "min_tls_version not set: $($_.Exception.Message)"
    }
} else {
    try {
        $cur = Invoke-CF GET "/zones/$zoneId/settings/min_tls_version"
        Write-Host "min_tls_version  : $($cur.result.value) (left as is)"
    } catch {
        Write-Host "min_tls_version  : left as is (not readable with this token)"
    }
}

# --- the response-header rule ---------------------------------------------
# Transform Rules live in the http_response_headers_transform phase. Rulesets
# are replaced wholesale, so read the existing one, drop any rule with our
# name, and put ours back -- otherwise re-running would stack duplicates.
$phase = 'http_response_headers_transform'
$existing = $null
try {
    $existing = Invoke-CF GET "/zones/$zoneId/rulesets/phases/$phase/entrypoint"
} catch {
    Write-Host "no existing $phase ruleset; creating one"
}

$rules = @()
if ($existing -and $existing.result.rules) {
    $rules = @($existing.result.rules | Where-Object { $_.description -ne $RuleName })
}

$actionParams = @{ headers = @{} }
foreach ($k in $headers.Keys) {
    $actionParams.headers[$k] = @{ operation = 'set'; value = $headers[$k] }
}

$rules += @{
    action            = 'rewrite'
    action_parameters = $actionParams
    expression        = 'true'          # all responses
    description       = $RuleName
    enabled           = $true
}

$body = @{ rules = $rules }
if ($existing) {
    Invoke-CF PUT "/zones/$zoneId/rulesets/$($existing.result.id)" $body | Out-Null
} else {
    Invoke-CF PUT "/zones/$zoneId/rulesets/phases/$phase/entrypoint" $body | Out-Null
}
Write-Host "rule deployed    : '$RuleName' ($($headers.Count) headers, all responses)"

$token = $null
[GC]::Collect()

# --- verify ----------------------------------------------------------------
# Poll rather than sleep once: the edge takes a variable few seconds to pick a
# new ruleset up, and a single early check reports a false failure on a rule
# that deployed perfectly well.
Write-Host "`nverifying (edge propagation takes a few seconds)..."
$deadline = (Get-Date).AddSeconds(60)
$missing = @($headers.Keys)
while ((Get-Date) -lt $deadline -and $missing.Count -gt 0) {
    Start-Sleep -Seconds 5
    try {
        # -UseBasicParsing: without it Windows PowerShell asks to run page
        # scripts through the IE engine, which stops an unattended run dead.
        $resp = Invoke-WebRequest "https://$Zone/" -Method Head -TimeoutSec 20 -UseBasicParsing
        $missing = @($headers.Keys | Where-Object { -not $resp.Headers[$_] })
        Write-Host ("  {0}/{1} present" -f ($headers.Count - $missing.Count), $headers.Count)
    } catch {
        Write-Host "  request failed: $($_.Exception.Message)"
    }
}
if ($missing.Count -eq 0) {
    Write-Host "all $($headers.Count) headers present" -ForegroundColor Green
} else {
    Write-Host "still missing after 60s: $($missing -join ', ')" -ForegroundColor Yellow
    Write-Host "the rule is deployed; check again with: curl -I https://$Zone/"
}
