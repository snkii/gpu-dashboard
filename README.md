# GPU Dashboard

A one-page dashboard for a lab's GPU servers: utilisation, memory, power and
temperature for every card, refreshed every second, readable on a phone.

Built for the Human Interface Laboratory at Seoul National University.

![tabs: servers, location, statistics](https://img.shields.io/badge/tabs-servers%20%C2%B7%20location%20%C2%B7%20statistics-458588)
![no dependencies](https://img.shields.io/badge/dependencies-none-98971a)
![license](https://img.shields.io/badge/license-MIT-d79921)

---

## Why it is built this way

Three constraints shaped the whole design, and none of them are obvious from
the screenshots.

**1. Opening SSH connections is the dangerous operation, not transferring data.**

Campus networks commonly flag a host that opens many SSH connections per
second. This one suspended the collector machine's network access after two
copies of an earlier version ran at once, each with its own in-process rate
limiter, reaching roughly five connections per second between them.

So the collector opens **one connection per server and holds it open**. A
restricted remote command emits a block of `nvidia-smi` output once a second
and the connection is never reopened while it is healthy. One-second data costs
zero connections in steady state. On top of that:

- a **cross-process** rate gate, backed by a file lock, so two processes cannot
  each think they are within budget (hard ceiling 2.0/sec, shipped at 0.40/sec)
- a single-instance lock, so a second collector exits instead of competing
- hosts whose key is not installed are disabled rather than retried, because
  repeated authentication failures are what trigger a `fail2ban` ban

`tests/test_rate_safety.py` asserts these bounds; it is the test that matters
most in this repository.

**2. Nothing on the public internet may reach the lab.**

The dashboard is not a server. The collector pushes a redacted JSON snapshot
*outbound* to object storage, and viewers read it from a CDN. There is no
inbound path to the collector machine or to any lab server — not a
firewalled one, not an authenticated one. None.

Redaction happens before the bytes are written, not in the browser: the public
snapshot never contains hostnames, addresses, ports, usernames, process names,
PIDs, owner fields, or SSH error strings. What cannot be sent cannot leak.

**3. The remote key can only run one command.**

The monitoring key is pinned in `authorized_keys` with
`command="...",restrict`. Asking that key to run anything else returns the
metrics output instead. Verified per host during provisioning.

---

## What it shows

| Tab | Contents |
|---|---|
| **Servers** | One card per machine: per-GPU utilisation, VRAM, power against limit, temperature and fan, plus a live power sparkline. Pin a card to keep it on top. |
| **Location** | Racks drawn as they are physically stacked, each slot coloured by draw against its power limit, with a per-room total-power trace. |
| **Statistics** | Per-server and per-rack history over 6 hours, 24 hours, 7 days or everything stored — power, utilisation, temperature, busy GPUs, plus mean/peak draw, energy used and duty cycle. |

An optional line at the top summarises the current state in prose. It is
off unless a key is configured — see below.

Gruvbox, dark by default, with a light toggle that persists. The page is a
single HTML file with no framework, no build step and no third-party requests.

---

## Architecture

```
 lab servers                collector machine                  viewers
┌────────────┐   ssh    ┌───────────────────────┐   https   ┌──────────┐
│ nvidia-smi │─────────▶│ hilmon (Rust)         │──────────▶│ object   │
│ 1 Hz block │  held    │  · rate gate + lock   │  outbound │ storage  │
└────────────┘  open    │  · redact             │    only   │ + CDN    │
                        │  · time-series store  │           └────┬─────┘
                        └───────────────────────┘                │
                                                                 ▼
                                                           one HTML page
```

The collector is written in Rust with **no dependencies at all** — the JSON
writer, HTTP server, file locking, date maths and time-series store are in
`rust/src/`, about 0.65 MB of binary. A Python implementation
(`hilmon.py`) predates it and still works; it is kept as a reference and as a
fallback launcher.

### The statistics store

Not SQLite. The access pattern is "append one row per server per tick, read a
time range", which a fixed-width columnar file serves faster and with nothing
to install. One file per server per day, sixteen bytes per row:

```
u32 ts │ u16 watts │ u16 watt_cap │ u8 util_avg │ u8 util_max
       │ u8 temp_max │ u8 gpus │ u8 busy │ u8 pad │ u16 mem_pct
```

Fixed width is the point: a range query is a binary search plus a sequential
read, retention is deleting files, and a long range is sampled by striding over
records rather than scanning them. Downsampling averages the averages but takes
the **maximum** of the maxima — an averaged peak is not a peak.

---

## Layout

```
hilmon.py                        Python collector (reference implementation)
ratelimit.py                     cross-process connection gate + single-instance lock
servers.example.json             inventory template; copy to servers.json
rust/src/
  main.rs                        CLI, publish loop
  collect.rs                     persistent SSH streams
  gate.rs                        rate gate, file locking
  db.rs                          time-series store
  model.rs  json.rs  http.rs  power.rs
web/index.html                   the entire dashboard
provision/                       account + restricted-key installation
deploy/                          storage config, security headers, daemon install
tests/test_rate_safety.py        connection-rate bounds
```

---

## Getting started

```bash
cp servers.example.json servers.json     # then edit it
cd rust && cargo build --release
./target/release/hilmon --check          # connect to each host, report, exit
./target/release/hilmon                  # serve on 127.0.0.1:8899
```

`servers.json` is gitignored: it names internal hosts, rooms and rack
positions.

| Command | Does |
|---|---|
| `hilmon --check` | Connect to every server, print a status line each, exit |
| `hilmon --once` | One snapshot as JSON on stdout |
| `hilmon --rate` | Print the connection-rate ceiling and recent history |
| `hilmon --publish DIR --loop` | Write `status.json` + `stats.json` + the page into DIR, forever |
| `--upload-cmd`, `--upload-cmd-tick`, `--upload-cmd-stats` | Commands run inside DIR after the first publish, after each tick, and after each statistics rewrite |

Publishing splits the uploads deliberately: `status.json` is small and changes
every tick, `stats.json` is large and changes once a minute, and the page
changes almost never. Syncing the whole directory every tick would bill many
operations for one changed file.

### Provisioning a server

```bash
python provision/provision.py --dry-run
python provision/provision.py --only gpu01
```

Creates the monitoring account with **no password** (key-only, no sudo),
installs the metrics script, and pins the monitoring key to a forced command.
Passwords for the privileged account are read at a masked prompt, held in
memory for that host only, and never written to a file, a command line or shell
history.

### The written summary (optional)

A sentence or two of prose at the top of the page, from Gemini. Off by default.
To enable it, put an API key in a file — one line, nothing else:

```
~/.hilgpu/gemini.key          (%USERPROFILE%\.hilgpu\gemini.key on Windows)
```

That path is outside the repository on purpose. **The key must never reach the
page**: a key shipped to the browser is a key handed to everyone who opens it.
The call therefore happens on the collector machine and only the resulting text
is published, as `summary.json`.

What is sent is built from the same redacted snapshot that is already public,
so the request reveals nothing a visitor could not read anyway. Note that free
API tiers commonly reserve the right to review submitted content — which is
tolerable precisely because this content is already public, and would not be if
the snapshot were not redacted.

The default cadence is one call every five minutes (288/day). `--summary-model`
picks the model and `--summary-interval` the cadence. A failed call leaves the
previous summary in place; a missing key simply hides the line.

### Deployment

`deploy/` covers object storage, HTTPS, security headers and a self-healing
background task. Site-specific identifiers — paths, bucket, account, domain —
live in `deploy/local.settings.ps1`, which is gitignored; copy
`local.settings.example.ps1` to create it. See `deploy/SECURITY.md` for the
threat model.

---

## Security notes

- The public snapshot is redacted at the source. Verify with
  `hilmon --once` and read what it actually contains.
- No credential is ever written to a file by this code. Storage keys go
  straight into the storage tool's own config with a restricted ACL; API tokens
  are used for one call and discarded.
- `servers.json`, `deploy/local.settings.ps1`, `bin/`, `out/`, `logs/` and
  `stats/` are gitignored. Check `.gitignore` before adding files.
- Never run two collectors. The lock prevents it; do not work around it.

---

## License

MIT — see [LICENSE](LICENSE).

Copyright (c) 2026 Seonuk Kim, Human Interface Laboratory, Seoul National
University.
