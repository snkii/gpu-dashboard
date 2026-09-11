// Copyright (c) 2026 Seonuk Kim, Human Interface Laboratory, Seoul National University
// SPDX-License-Identifier: MIT
//! Time-series store for the statistics tab.
//!
//! Not SQLite: the access pattern is append-one-row-per-server-per-tick and
//! read-a-time-range, which a fixed-width columnar file does faster and with
//! no dependency. One file per server per day keeps a range query to a couple
//! of sequential reads and makes retention a matter of deleting files.
//!
//! Record layout, little-endian, fixed 16 bytes so offset == index * 16:
//!     u32  ts        unix seconds
//!     u16  watts     rounded total draw
//!     u16  watt_cap  summed configured power limit
//!     u8   util_avg  mean utilisation, 0..100
//!     u8   util_max  0..100
//!     u8   temp_max  degrees C
//!     u8   gpus      cards reporting
//!     u8   busy      cards busy
//!     u8   _pad
//!     u16  mem_pct   thousandths of total VRAM in use, 0..1000
//!
//! Fixed width is the point: a range scan is a bisection plus a linear read.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

pub const REC: usize = 16;

/// Windows the statistics tab offers, with how many slots each is drawn at.
/// Slot widths work out to 2 minutes, 10 minutes and 1 hour.
pub const STATS_RANGES: [(&str, u32, usize); 3] = [
    ("6h", 6 * 3600, 180),
    ("24h", 24 * 3600, 144),
    ("7d", 7 * 86_400, 168),
];

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Row {
    pub ts: u32,
    pub watts: u16,
    pub watt_cap: u16,
    pub util_avg: u8,
    pub util_max: u8,
    pub temp_max: u8,
    pub gpus: u8,
    pub busy: u8,
    pub mem_pct: u16,
}

impl Row {
    pub fn encode(&self) -> [u8; REC] {
        let mut b = [0u8; REC];
        b[0..4].copy_from_slice(&self.ts.to_le_bytes());
        b[4..6].copy_from_slice(&self.watts.to_le_bytes());
        b[6..8].copy_from_slice(&self.watt_cap.to_le_bytes());
        b[8] = self.util_avg;
        b[9] = self.util_max;
        b[10] = self.temp_max;
        b[11] = self.gpus;
        b[12] = self.busy;
        b[13] = 0;
        b[14..16].copy_from_slice(&self.mem_pct.to_le_bytes());
        b
    }

    pub fn decode(b: &[u8]) -> Option<Row> {
        if b.len() < REC {
            return None;
        }
        Some(Row {
            ts: u32::from_le_bytes(b[0..4].try_into().ok()?),
            watts: u16::from_le_bytes(b[4..6].try_into().ok()?),
            watt_cap: u16::from_le_bytes(b[6..8].try_into().ok()?),
            util_avg: b[8],
            util_max: b[9],
            temp_max: b[10],
            gpus: b[11],
            busy: b[12],
            mem_pct: u16::from_le_bytes(b[14..16].try_into().ok()?),
        })
    }
}

pub struct Db {
    dir: PathBuf,
    // One open writer per server per day. Reopening on every sample would be a
    // syscall per server per second for no gain.
    open: BTreeMap<String, (String, BufWriter<File>)>,
    last_write: BTreeMap<String, u32>,
    period: u32,
}

impl Db {
    /// `period` seconds between stored rows. Keeping every 1 Hz sample is
    /// 1.4 MB per server per day and nothing in the statistics view can
    /// resolve it; at 10s a server costs about 50 MB per year.
    pub fn open(dir: PathBuf, period: u32) -> Db {
        let _ = fs::create_dir_all(&dir);
        Db {
            dir,
            open: BTreeMap::new(),
            last_write: BTreeMap::new(),
            period: period.max(1),
        }
    }

    fn path(&self, server: &str, day: &str) -> PathBuf {
        self.dir.join(format!("{}-{}.bin", server, day))
    }

    pub fn append(&mut self, server: &str, row: Row) -> std::io::Result<bool> {
        if let Some(&last) = self.last_write.get(server) {
            if row.ts < last + self.period {
                return Ok(false);
            }
        }
        let day = crate::power::day_of(row.ts as i64);
        let need_new = match self.open.get(server) {
            Some((d, _)) => *d != day,
            None => true,
        };
        if need_new {
            let f = OpenOptions::new()
                .create(true)
                .append(true)
                .open(self.path(server, &day))?;
            self.open
                .insert(server.to_string(), (day.clone(), BufWriter::new(f)));
        }
        if let Some((_, w)) = self.open.get_mut(server) {
            w.write_all(&row.encode())?;
            // Flush every row: this file exists to survive a crash or a power
            // cut, and a row is 16 bytes.
            w.flush()?;
        }
        self.last_write.insert(server.to_string(), row.ts);
        Ok(true)
    }

    /// Rows for one server between two unix timestamps, inclusive.
    pub fn range(&self, server: &str, from: u32, to: u32) -> Vec<Row> {
        self.range_strided(server, from, to, usize::MAX)
    }

    /// Rows between two timestamps, reading at most about `want` of them.
    ///
    /// The all-time range can span a year -- three million rows per server,
    /// re-read every time the statistics file is rebuilt. Because records are
    /// fixed width, taking every Nth one is a seek rather than a scan, and the
    /// result is indistinguishable once it has been averaged into a few
    /// hundred buckets.
    pub fn range_strided(&self, server: &str, from: u32, to: u32, want: usize) -> Vec<Row> {
        let days = days_between(from as i64, to as i64);
        let mut counts = Vec::with_capacity(days.len());
        let mut total = 0usize;
        for day in &days {
            let n = fs::metadata(self.path(server, day))
                .map(|m| m.len() as usize / REC)
                .unwrap_or(0);
            counts.push(n);
            total += n;
        }
        let stride = if want == 0 || total <= want { 1 } else { total / want };

        let mut out = Vec::new();
        for (day, &n) in days.iter().zip(counts.iter()) {
            if n == 0 {
                continue;
            }
            let mut f = match File::open(self.path(server, day)) {
                Ok(f) => f,
                Err(_) => continue,
            };
            // Records are fixed width AND appended in time order, so the first
            // row >= `from` is found by bisection rather than by reading the
            // whole day.
            let start = bisect(&mut f, n, from);
            if start >= n {
                continue;
            }
            if stride == 1 {
                if f.seek(SeekFrom::Start((start * REC) as u64)).is_err() {
                    continue;
                }
                let mut buf = Vec::new();
                let _ = f.read_to_end(&mut buf);
                for chunk in buf.chunks_exact(REC) {
                    match Row::decode(chunk) {
                        Some(r) if r.ts > to => break,
                        Some(r) if r.ts >= from => out.push(r),
                        _ => {}
                    }
                }
            } else {
                let mut buf = [0u8; REC];
                let mut i = start;
                while i < n {
                    if f.seek(SeekFrom::Start((i * REC) as u64)).is_err()
                        || f.read_exact(&mut buf).is_err()
                    {
                        break;
                    }
                    match Row::decode(&buf) {
                        Some(r) if r.ts > to => break,
                        Some(r) if r.ts >= from => out.push(r),
                        _ => {}
                    }
                    i += stride;
                }
            }
        }
        out
    }

    /// Timestamp of the oldest row on disk.
    ///
    /// The oldest day is found from the file names, then its first record is
    /// read for the actual timestamp. Using the day boundary instead would
    /// start the all-time chart at midnight UTC on the first day and draw
    /// hours of blank space before collection began.
    pub fn earliest(&self) -> Option<u32> {
        let mut best: Option<(String, PathBuf)> = None;
        let mut files: Vec<(String, PathBuf)> = Vec::new();
        for e in fs::read_dir(&self.dir).ok()?.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let day = match name.rsplit('-').next().and_then(|s| s.strip_suffix(".bin")) {
                Some(d) if d.len() == 8 && d.bytes().all(|b| b.is_ascii_digit()) => d.to_string(),
                _ => continue,
            };
            if best.as_ref().map_or(true, |(b, _)| day < *b) {
                best = Some((day.clone(), e.path()));
            }
            files.push((day, e.path()));
        }
        // Only the oldest day can hold the oldest row.
        let oldest_day = best.as_ref()?.0.clone();
        files.retain(|(d, _)| *d == oldest_day);
        let (day, _) = best.clone()?;
        // Read the first row of EVERY file and take the smallest. Reading only
        // the oldest file's would fall back to midnight whenever that one file
        // happened to be empty or unreadable, which silently prepends hours of
        // blank space to the all-time chart.
        let mut min: Option<u32> = None;
        for (_, path) in &files {
            let mut buf = [0u8; REC];
            if let Ok(mut f) = File::open(path) {
                if f.read_exact(&mut buf).is_ok() {
                    if let Some(r) = Row::decode(&buf) {
                        min = Some(min.map_or(r.ts, |m: u32| m.min(r.ts)));
                    }
                }
            }
        }
        if min.is_some() {
            return min;
        }
        // Every file empty or unreadable: fall back to the oldest day name.
        let y: i64 = day[0..4].parse().ok()?;
        let m: u32 = day[4..6].parse().ok()?;
        let d: u32 = day[6..8].parse().ok()?;
        Some((crate::power::days_from_civil(y, m, d) * 86_400) as u32)
    }

    /// Delete day files older than `keep_days`.
    pub fn prune(&self, keep_days: i64, now_ts: i64) -> usize {
        let cutoff = crate::power::day_of(now_ts - keep_days * 86_400);
        let mut n = 0;
        if let Ok(rd) = fs::read_dir(&self.dir) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if let Some(day) = name.rsplit('-').next().and_then(|s| s.strip_suffix(".bin")) {
                    if day < cutoff.as_str() && fs::remove_file(e.path()).is_ok() {
                        n += 1;
                    }
                }
            }
        }
        n
    }

    pub fn size_bytes(&self) -> u64 {
        let mut total = 0;
        if let Ok(rd) = fs::read_dir(&self.dir) {
            for e in rd.flatten() {
                total += e.metadata().map(|m| m.len()).unwrap_or(0);
            }
        }
        total
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

/// Index of the first record whose ts >= target.
fn bisect(f: &mut File, n: usize, target: u32) -> usize {
    let (mut lo, mut hi) = (0usize, n);
    let mut buf = [0u8; REC];
    while lo < hi {
        let mid = (lo + hi) / 2;
        if f.seek(SeekFrom::Start((mid * REC) as u64)).is_err() {
            return lo;
        }
        if f.read_exact(&mut buf).is_err() {
            return lo;
        }
        match Row::decode(&buf) {
            Some(r) if r.ts < target => lo = mid + 1,
            _ => hi = mid,
        }
    }
    lo
}

fn days_between(from: i64, to: i64) -> Vec<String> {
    let mut v = Vec::new();
    let mut d = from - from.rem_euclid(86_400);
    while d <= to {
        v.push(crate::power::day_of(d));
        d += 86_400;
    }
    if v.is_empty() {
        v.push(crate::power::day_of(from));
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let p = std::env::temp_dir()
            .join(format!("hilmon-db-{}-{}", tag, std::process::id()));
        let _ = fs::remove_dir_all(&p);
        p
    }

    #[test]
    fn row_round_trips() {
        let r = Row {
            ts: 1_789_000_000,
            watts: 1551,
            watt_cap: 2000,
            util_avg: 73,
            util_max: 100,
            temp_max: 83,
            gpus: 8,
            busy: 6,
            mem_pct: 812,
        };
        assert_eq!(Row::decode(&r.encode()), Some(r));
        assert_eq!(r.encode().len(), REC);
    }

    #[test]
    fn append_respects_the_period_and_range_reads_back() {
        let dir = tmpdir("append");
        let mut db = Db::open(dir.clone(), 10);
        let base = 1_789_000_000u32;
        let mut written = 0;
        for i in 0..30u32 {
            let r = Row { ts: base + i, watts: 100 + i as u16, gpus: 4, ..Default::default() };
            if db.append("hi14", r).unwrap() {
                written += 1;
            }
        }
        assert_eq!(written, 3, "a 10s period must throttle 30 one-second samples");

        let got = db.range("hi14", base, base + 100);
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].watts, 100);
        assert_eq!(got[1].watts, 110);
        assert_eq!(got[2].ts, base + 20);

        let tail = db.range("hi14", base + 15, base + 100);
        assert_eq!(tail.len(), 1, "range must exclude rows before `from`");
        assert_eq!(tail[0].ts, base + 20);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn strided_reads_sample_across_the_whole_span() {
        let dir = tmpdir("stride");
        let mut db = Db::open(dir.clone(), 1);
        let base = 1_789_000_000u32;
        for i in 0..1000u32 {
            db.append("hi14", Row { ts: base + i, watts: i as u16, ..Default::default() })
                .unwrap();
        }
        let all = db.range("hi14", base, base + 1000);
        assert_eq!(all.len(), 1000);

        let few = db.range_strided("hi14", base, base + 1000, 50);
        assert!(few.len() <= 60, "got {}", few.len());
        assert!(few.len() >= 40, "got {}", few.len());
        // The point of striding is coverage, not a prefix: the last sample must
        // still come from near the end of the span.
        assert!(few[0].ts < base + 40);
        assert!(few[few.len() - 1].ts > base + 940);
        assert!(few.windows(2).all(|w| w[0].ts < w[1].ts));

        assert_eq!(db.earliest(), Some(base), "earliest is the first row, not midnight");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn bisect_finds_the_first_row_at_or_after() {
        let dir = tmpdir("bisect");
        let mut db = Db::open(dir.clone(), 1);
        let base = 1_789_000_000u32;
        for i in 0..50u32 {
            db.append("hi20", Row { ts: base + i * 10, watts: i as u16, ..Default::default() })
                .unwrap();
        }
        let got = db.range("hi20", base + 205, base + 235);
        assert_eq!(
            got.iter().map(|r| r.ts).collect::<Vec<_>>(),
            vec![base + 210, base + 220, base + 230]
        );
        let _ = fs::remove_dir_all(&dir);
    }
}

/// Downsample onto a FIXED time grid: slot `i` always covers
/// `[from + i*step, from + (i+1)*step)`, for every server alike.
///
/// Chunk-based bucketing would give each server its own timestamps, and the
/// statistics page sums servers into racks -- which is only correct if slot
/// `i` means the same instant everywhere. Slots with no data come back as
/// `Row::default()`, recognisable by `gpus == 0`.
pub fn grid(rows: &[Row], from: u32, step: u32, n: usize) -> Vec<Row> {
    let step = step.max(1);
    let mut acc: Vec<Option<(u32, u64, u64, u32, u8, u8, u32, u64)>> = vec![None; n];
    let mut gpus_seen: Vec<u8> = vec![0; n];
    for r in rows {
        if r.ts < from {
            continue;
        }
        let i = ((r.ts - from) / step) as usize;
        if i >= n {
            continue;
        }
        let e = acc[i].get_or_insert((0, 0, 0, 0, 0, 0, 0, 0));
        e.0 += 1;
        e.1 += r.watts as u64;
        e.2 += r.util_avg as u64;
        e.3 = e.3.max(r.watt_cap as u32);
        e.4 = e.4.max(r.util_max);
        e.5 = e.5.max(r.temp_max);
        e.6 += r.busy as u32;
        e.7 += r.mem_pct as u64;
        if r.gpus > gpus_seen[i] {
            gpus_seen[i] = r.gpus;
        }
    }
    acc.into_iter()
        .enumerate()
        .map(|(i, e)| match e {
            Some((c, w, ua, cap, umax, tmax, busy, mem)) if c > 0 => Row {
                ts: from + i as u32 * step,
                watts: (w / c as u64) as u16,
                watt_cap: cap as u16,
                util_avg: (ua / c as u64) as u8,
                util_max: umax,
                temp_max: tmax,
                gpus: gpus_seen[i],
                busy: ((busy + c / 2) / c) as u8,
                mem_pct: (mem / c as u64) as u16,
            },
            _ => Row { ts: from + i as u32 * step, ..Default::default() },
        })
        .collect()
}

#[cfg(test)]
mod grid_tests {
    use super::*;

    #[test]
    fn grid_is_aligned_and_marks_gaps() {
        let rows = vec![
            Row { ts: 1000, watts: 100, util_avg: 10, util_max: 90, gpus: 4, busy: 2, ..Default::default() },
            Row { ts: 1005, watts: 200, util_avg: 30, util_max: 10, gpus: 4, busy: 2, ..Default::default() },
            // nothing in slot 1
            Row { ts: 1020, watts: 300, util_avg: 50, util_max: 50, gpus: 4, busy: 4, ..Default::default() },
        ];
        let g = grid(&rows, 1000, 10, 3);
        assert_eq!(g.len(), 3);
        assert_eq!(g[0].ts, 1000);
        assert_eq!(g[0].watts, 150, "slot averages its samples");
        assert_eq!(g[0].util_max, 90, "peak survives");
        assert_eq!(g[1].gpus, 0, "an empty slot is flagged by gpus == 0");
        assert_eq!(g[2].ts, 1020);
        assert_eq!(g[2].watts, 300);
        // Every server gets the same slot timestamps -- the property racks rely on.
        let other = grid(&[Row { ts: 1021, watts: 7, gpus: 2, ..Default::default() }], 1000, 10, 3);
        assert_eq!(
            g.iter().map(|r| r.ts).collect::<Vec<_>>(),
            other.iter().map(|r| r.ts).collect::<Vec<_>>()
        );
    }
}
