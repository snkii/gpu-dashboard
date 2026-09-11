# Copyright (c) 2026 Seonuk Kim, Human Interface Laboratory, Seoul National University
# SPDX-License-Identifier: MIT
<#
    Store a Gemini API key for the written summary.

    The key is typed at a masked prompt and written straight to a file that only
    your account can read. It is never echoed, never placed on a command line
    (where any process could read it out of the argument list), and never put in
    shell history. Nothing else in this repository reads it except the collector.

    Get a key at https://aistudio.google.com/apikey -- the free tier is enough.

    Usage:
        .\deploy\Set-GeminiKey.ps1
        .\deploy\Set-GeminiKey.ps1 -Remove     # turn the summary back off
        .\deploy\Set-GeminiKey.ps1 -Verify     # test the stored key
#>
[CmdletBinding()]
param(
    [string] $Model = 'gemini-2.5-flash',
    [switch] $Remove,
    [switch] $Verify
)

$ErrorActionPreference = 'Stop'

$dir  = Join-Path $env:USERPROFILE '.hilgpu'
$path = Join-Path $dir 'gemini.key'

if ($Remove) {
    if (Test-Path $path) {
        Remove-Item $path -Force
        Write-Host "removed $path -- the summary is now off." -ForegroundColor Yellow
    } else {
        Write-Host "nothing to remove; the summary was already off."
    }
    return
}

New-Item -ItemType Directory -Force -Path $dir | Out-Null

if (-not $Verify) {
    Write-Host "Paste the Gemini API key. Input is masked and is not echoed."
    Write-Host "  (get one at https://aistudio.google.com/apikey)`n"
    $secure = Read-Host "Gemini API key" -AsSecureString
    $bstr   = [Runtime.InteropServices.Marshal]::SecureStringToBSTR($secure)
    try { $key = [Runtime.InteropServices.Marshal]::PtrToStringBSTR($bstr) }
    finally { [Runtime.InteropServices.Marshal]::ZeroFreeBSTR($bstr) }

    if (-not $key) { throw 'no key entered' }
    $key = $key.Trim()

    # Only catch a paste that went wrong. Key formats change and are not
    # documented as a grammar, so the authority on whether a key is valid is
    # the API -- which this script calls at the end. A regex that guesses the
    # format just rejects keys that would have worked.
    if ($key -match '\s') {
        throw 'the key contains a space or newline -- it did not paste cleanly'
    }
    if ($key -match '[^\x21-\x7E]') {
        throw 'the key contains non-printable characters -- it did not paste cleanly'
    }
    if ($key.Length -lt 20) {
        throw "only $($key.Length) characters -- that looks truncated"
    }

    # -NoNewline: a trailing newline is tolerated by the reader, but a file that
    # holds exactly the key is easier to reason about.
    Set-Content -Path $path -Value $key -Encoding ascii -NoNewline

    # Readable only by this account. The default would inherit the profile's
    # ACL, which is already user-only, but this file deserves to be explicit.
    $acl = New-Object System.Security.AccessControl.FileSecurity
    $acl.SetAccessRuleProtection($true, $false)
    $acl.AddAccessRule((New-Object System.Security.AccessControl.FileSystemAccessRule(
        $env:USERNAME, 'FullControl', 'Allow')))
    Set-Acl -Path $path -AclObject $acl

    Write-Host "`nstored: $path" -ForegroundColor Green
    Write-Host "length: $($key.Length) characters (the key itself is not shown)"
    $key = $null
}

if (-not (Test-Path $path)) { throw "no key stored at $path" }

Write-Host "`nchecking the key against $Model ..."
$body = @{ contents = @(@{ parts = @(@{ text = 'ping' }) }) } | ConvertTo-Json -Depth 6 -Compress
$bodyFile = Join-Path $env:TEMP "gemini-check-$PID.json"
$cfgFile  = Join-Path $env:TEMP "gemini-check-$PID.curl"
Set-Content -Path $bodyFile -Value $body -Encoding utf8 -NoNewline

# Same reasoning as the collector: the key goes in a curl config file, not on
# the command line.
#
# Forward slashes: inside a curl config file a backslash is an escape
# character, so a Windows path written verbatim arrives as C:Userssukim...
# and the file cannot be opened. Windows accepts forward slashes everywhere.
$bodyArg = $bodyFile.Replace('\', '/')
$stored = (Get-Content $path -Raw).Trim()
@(
    "url = `"https://generativelanguage.googleapis.com/v1beta/models/$Model`:generateContent`""
    'header = "Content-Type: application/json"'
    "header = `"x-goog-api-key: $stored`""
    "data-binary = `"@$bodyArg`""
    'request = "POST"'
    'silent'
    'show-error'
    'max-time = 30'
) | Set-Content -Path $cfgFile -Encoding ascii
$stored = $null

try {
    $out = & curl.exe --config $cfgFile 2>&1 | Out-String
} finally {
    Remove-Item $cfgFile, $bodyFile -Force -ErrorAction SilentlyContinue
}

if ($out -match '"error"') {
    $msg = ([regex]::Match($out, '"message"\s*:\s*"([^"]+)"')).Groups[1].Value
    $code = ([regex]::Match($out, '"code"\s*:\s*(\d+)')).Groups[1].Value
    Write-Host "FAILED ($code): $msg" -ForegroundColor Red
    Write-Host ""
    if ($code -eq '404') {
        Write-Host "The model name may be wrong. Try -Model gemini-2.0-flash."
    } elseif ($code -eq '400' -or $code -eq '403') {
        Write-Host "The key was rejected. Re-run without -Verify and paste it again."
    } elseif ($code -eq '429') {
        Write-Host "The key works, but the quota is currently exhausted."
    }
    exit 1
}
if ($out -match '"text"') {
    Write-Host "OK -- $Model answered. The summary will appear within ten minutes." -ForegroundColor Green
} else {
    Write-Host "unexpected response:" -ForegroundColor Yellow
    Write-Host ($out.Substring(0, [Math]::Min(300, $out.Length)))
    exit 1
}
