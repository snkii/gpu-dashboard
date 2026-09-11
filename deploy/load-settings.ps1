# Copyright (c) 2026 Seonuk Kim, Human Interface Laboratory, Seoul National University
# SPDX-License-Identifier: MIT
#
# Load deployment settings that must not be published.
#
# Everything here names something real -- a path on this machine, a storage
# bucket, a Cloudflare account, a domain. None of it is a password, but all of
# it identifies the deployment, so it lives in local.settings.ps1, which is
# gitignored. Copy local.settings.example.ps1 to get started.
#
# Returns a hashtable. Callers use it to fill in whichever parameters the
# operator did not pass explicitly.

$path = Join-Path $PSScriptRoot 'local.settings.ps1'
if (-not (Test-Path $path)) {
    throw @"
deploy/local.settings.ps1 is missing.

    Copy-Item deploy/local.settings.example.ps1 deploy/local.settings.ps1
    # then edit it

It is deliberately not in the repository: it names your machine, your storage
bucket and your Cloudflare account.
"@
}

$settings = & $path
if ($settings -isnot [hashtable]) {
    throw "deploy/local.settings.ps1 must end with a hashtable literal; got $($settings.GetType().Name)."
}
$settings
