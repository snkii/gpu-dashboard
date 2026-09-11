// Copyright (c) 2026 Seonuk Kim, Human Interface Laboratory, Seoul National University
// SPDX-License-Identifier: MIT
//! Per-server power history: a rolling window for the page, a CSV on disk.

use crate::gate::now;
use crate::model::Gpu;
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

pub struct PowerLog {
    points: usize,
    enabled: bool,
    hist: BTreeMap<String, Vec<f64>>,
    dir: PathBuf,
    day: String,
    file: Option<File>,
}

impl PowerLog {
    pub fn new(points: usize, enabled: bool) -> PowerLog {
        let dir = std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join("logs");
        if enabled {
            let _ = fs::create_dir_all(&dir);
        }
        PowerLog {
            points: points.max(2),
            enabled,
            hist: BTreeMap::new(),
            dir,
            day: String::new(),
            file: None,
        }
    }

    pub fn record(&mut self, name: &str, watts: f64, gpus: &[Gpu]) {
        let h = self.hist.entry(name.to_string()).or_default();
        h.push(watts);
        if h.len() > self.points {
            let cut = h.len() - self.points;
            h.drain(..cut);
        }
        if self.enabled {
            if let Err(e) = self.append(name, gpus) {
                // Logging must never take the collector down.
                eprintln!("[powerlog] {}", e);
                self.enabled = false;
            }
        }
    }

    fn append(&mut self, name: &str, gpus: &[Gpu]) -> std::io::Result<()> {
        // One file per day so it stays greppable and never grows unbounded.
        let day = today();
        if day != self.day {
            let path = self.dir.join(format!("power-{}.csv", day));
            let fresh = !path.exists();
            let mut f = OpenOptions::new().create(true).append(true).open(&path)?;
            if fresh {
                writeln!(f, "ts,server,gpu_index,watts,watt_limit,util,temp")?;
            }
            self.file = Some(f);
            self.day = day;
        }
        let ts = now() as u64;
        if let Some(f) = self.file.as_mut() {
            for g in gpus {
                writeln!(
                    f,
                    "{},{},{},{},{},{},{}",
                    ts,
                    name,
                    g.index,
                    fmt(g.power),
                    fmt(g.power_limit),
                    fmt(g.util),
                    fmt(g.temp)
                )?;
            }
            f.flush()?;
        }
        Ok(())
    }

    pub fn series(&self, name: &str) -> Vec<f64> {
        self.hist.get(name).cloned().unwrap_or_default()
    }
}

fn fmt(v: Option<f64>) -> String {
    match v {
        Some(x) if x.fract() == 0.0 => format!("{}", x as i64),
        Some(x) => format!("{:.1}", x),
        None => String::new(),
    }
}

/// Local date as YYYYMMDD, derived from the unix clock.
///
/// Written out rather than pulled from a crate: this is the only date
/// formatting the program does, and a dependency for it is not worth it.
fn today() -> String {
    day_of(now() as i64)
}

/// YYYYMMDD for a unix timestamp. Shared with the statistics store, which
/// names its files by day.
pub fn day_of(secs: i64) -> String {
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    format!("{:04}{:02}{:02}", y, m, d)
}

/// Howard Hinnant's days-from-civil. Turns a YYYYMMDD file name back into a
/// timestamp, which is how the all-time range finds where the data starts.
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let mp = if m > 2 { m - 3 } else { m + 9 } as u64;
    let doy = (153 * mp + 2) / 5 + d as u64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe as i64 - 719_468
}

/// Howard Hinnant's days-from-civil, inverted. Valid for any Gregorian date.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_keeps_only_the_last_n() {
        let mut p = PowerLog::new(3, false);
        for w in [1.0, 2.0, 3.0, 4.0, 5.0] {
            p.record("hi14", w, &[]);
        }
        assert_eq!(p.series("hi14"), vec![3.0, 4.0, 5.0]);
        assert!(p.series("nope").is_empty());
    }

    #[test]
    fn civil_conversion_round_trips() {
        for days in [0i64, 19_723, 19_784, -1, 25_000] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days, "{:?}", (y, m, d));
        }
    }

    #[test]
    fn civil_dates_are_right() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1));
        assert_eq!(civil_from_days(19_784), (2024, 3, 2)); // just after a leap day
    }

    #[test]
    fn csv_fields_drop_pointless_decimals() {
        assert_eq!(fmt(Some(250.0)), "250");
        assert_eq!(fmt(Some(249.46)), "249.5");
        assert_eq!(fmt(None), "");
    }
}
