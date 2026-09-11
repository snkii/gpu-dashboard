# Copyright (c) 2026 Seonuk Kim, Human Interface Laboratory, Seoul National University
# SPDX-License-Identifier: MIT
#
# Copy to local.settings.ps1 and fill in. That copy is gitignored.
#
# None of these are passwords -- the R2 access key and secret are never stored
# here; Set-RcloneConfig.ps1 prompts for them and writes them straight into
# rclone's own config with a restricted ACL. These are identifiers, and they
# are kept out of the repository because together they point at one specific
# machine and one specific account.

@{
    # Where this checkout lives. The scheduled tasks need an absolute path.
    Root = 'C:\Users\you\gpu-dashboard'

    # rclone remote name and bucket, as configured by Set-RcloneConfig.ps1.
    Remote = 'gpudash'
    Bucket = 'gpudash'

    # Cloudflare account that owns the R2 bucket. Shown in the dashboard URL and
    # in the R2 endpoint hostname.
    AccountId = '<32-hex-cloudflare-account-id>'

    # The domain the dashboard is published on.
    Zone = 'gpu.example.edu'
}
