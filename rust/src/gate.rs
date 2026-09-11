// Copyright (c) 2026 Seonuk Kim, Human Interface Laboratory, Seoul National University
// SPDX-License-Identifier: MIT
//! Machine-wide ceiling on outbound SSH connections.
//!
//! Same contract as ratelimit.py, and deliberately the SAME state file: while
//! both implementations exist they must share one budget, or running the Rust
//! collector alongside a Python one would double the real rate -- which is the
//! exact mistake that got this machine's network suspended once already.
//!
//! Two rules, both enforced:
//!   1. MIN_GAP seconds between any two connections.
//!   2. at most MAX_PER_WINDOW connections in any WINDOW seconds.
//!
//! The waiting happens while holding an exclusive lock on the state file, so
//! the limit holds ACROSS processes rather than merely within one.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The campus threshold is ~60/sec. Everything here sits far below it on
/// purpose: bursting anywhere near the limit is what caused the suspension.
pub const HARD_CEILING_PER_SEC: f64 = 2.0;
pub const DEFAULT_MIN_GAP: f64 = 2.5;
pub const DEFAULT_WINDOW: f64 = 10.0;
pub const DEFAULT_MAX_PER_WINDOW: usize = 5;

pub fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

pub fn state_dir() -> PathBuf {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".hilgpu")
}

pub struct Gate {
    pub min_gap: f64,
    pub window: f64,
    pub max_per_window: usize,
    path: PathBuf,
}

impl Gate {
    pub fn new(min_gap: f64, window: f64, max_per_window: usize) -> Self {
        let window = window.max(1.0);
        // Clamp against the hard ceiling whatever the config asked for, so a
        // later edit to servers.json cannot quietly remove the protection.
        let min_gap = min_gap.max(1.0 / HARD_CEILING_PER_SEC);
        let allowed = (window * HARD_CEILING_PER_SEC) as usize;
        let max_per_window = max_per_window.max(1).min(allowed.max(1));
        let dir = state_dir();
        let _ = fs::create_dir_all(&dir);
        Gate {
            min_gap,
            window,
            max_per_window,
            path: dir.join("ssh-gate.json"),
        }
    }

    pub fn rate_per_sec(&self) -> f64 {
        (1.0 / self.min_gap).min(self.max_per_window as f64 / self.window)
    }

    /// Block until another connection is permitted.
    pub fn acquire(&self) {
        loop {
            let wait = match self.try_take() {
                Ok(w) => w,
                Err(_) => {
                    // A transient file error must not become an unthrottled
                    // free-for-all; fall back to the conservative gap.
                    thread::sleep(Duration::from_secs_f64(self.min_gap));
                    return;
                }
            };
            if wait <= 0.0 {
                return;
            }
            thread::sleep(Duration::from_secs_f64(wait.min(5.0) + 0.01));
        }
    }

    /// Returns 0.0 when the slot was taken, else how long to wait.
    fn try_take(&self) -> std::io::Result<f64> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(&self.path)?;
        let mut lock = FileLock::acquire(file)?;
        let f = lock.file_mut();

        let mut raw = String::new();
        f.seek(SeekFrom::Start(0))?;
        f.read_to_string(&mut raw)?;
        let mut hist = parse_stamps(&raw);

        let t = now();
        hist.retain(|&x| t - x < self.window * 3.0);
        if hist.len() > 200 {
            let cut = hist.len() - 200;
            hist.drain(..cut);
        }

        let mut wait: f64 = 0.0;
        if let Some(&last) = hist.iter().max_by(|a, b| a.partial_cmp(b).unwrap()) {
            wait = wait.max(self.min_gap - (t - last));
        }
        let recent: Vec<f64> = hist.iter().copied().filter(|&x| t - x < self.window).collect();
        if recent.len() >= self.max_per_window {
            let oldest = recent.iter().cloned().fold(f64::INFINITY, f64::min);
            wait = wait.max(self.window - (t - oldest));
        }
        if wait > 0.0 {
            return Ok(wait);
        }

        hist.push(t);
        let out = render_stamps(&hist);
        f.seek(SeekFrom::Start(0))?;
        f.set_len(0)?;
        f.write_all(out.as_bytes())?;
        f.flush()?;
        Ok(0.0)
    }
}

fn parse_stamps(raw: &str) -> Vec<f64> {
    // The file is a JSON array of numbers written by either implementation.
    raw.trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .split(',')
        .filter_map(|s| s.trim().parse::<f64>().ok())
        .collect()
}

fn render_stamps(v: &[f64]) -> String {
    let mut s = String::from("[");
    for (i, x) in v.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&format!("{:.3}", x));
    }
    s.push(']');
    s
}

/// Peak connections seen in any one-second window. Used by `--rate`.
pub fn peak_per_sec(mut hist: Vec<f64>) -> usize {
    hist.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let mut peak = 0usize;
    let mut j = 0usize;
    for i in 0..hist.len() {
        while hist[i] - hist[j] > 1.0 {
            j += 1;
        }
        peak = peak.max(i - j + 1);
    }
    peak
}

pub fn history() -> Vec<f64> {
    fs::read_to_string(state_dir().join("ssh-gate.json"))
        .map(|s| parse_stamps(&s))
        .unwrap_or_default()
}

// ------------------------------------------------------------- file locking

#[cfg(windows)]
mod sys {
    use std::fs::File;
    use std::os::windows::io::AsRawHandle;

    // LockFileEx / UnlockFileEx, declared directly so the crate stays
    // dependency-free.
    #[repr(C)]
    struct Overlapped {
        internal: usize,
        internal_high: usize,
        offset: u32,
        offset_high: u32,
        h_event: *mut core::ffi::c_void,
    }

    extern "system" {
        fn LockFileEx(
            handle: *mut core::ffi::c_void,
            flags: u32,
            reserved: u32,
            bytes_low: u32,
            bytes_high: u32,
            overlapped: *mut Overlapped,
        ) -> i32;
        fn UnlockFileEx(
            handle: *mut core::ffi::c_void,
            reserved: u32,
            bytes_low: u32,
            bytes_high: u32,
            overlapped: *mut Overlapped,
        ) -> i32;
    }

    const EXCLUSIVE: u32 = 0x0000_0002;
    const FAIL_IMMEDIATELY: u32 = 0x0000_0001;

    fn overlapped() -> Overlapped {
        Overlapped {
            internal: 0,
            internal_high: 0,
            offset: 0,
            offset_high: 0,
            h_event: std::ptr::null_mut(),
        }
    }

    pub fn lock(f: &File, blocking: bool) -> std::io::Result<bool> {
        let flags = if blocking { EXCLUSIVE } else { EXCLUSIVE | FAIL_IMMEDIATELY };
        let mut ov = overlapped();
        let ok = unsafe { LockFileEx(f.as_raw_handle() as *mut _, flags, 0, 1, 0, &mut ov) };
        if ok != 0 {
            Ok(true)
        } else if blocking {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(false)
        }
    }

    pub fn unlock(f: &File) {
        let mut ov = overlapped();
        unsafe {
            UnlockFileEx(f.as_raw_handle() as *mut _, 0, 1, 0, &mut ov);
        }
    }
}

#[cfg(unix)]
mod sys {
    use std::fs::File;
    use std::os::unix::io::AsRawFd;

    extern "C" {
        fn flock(fd: i32, operation: i32) -> i32;
    }
    const LOCK_EX: i32 = 2;
    const LOCK_NB: i32 = 4;
    const LOCK_UN: i32 = 8;

    pub fn lock(f: &File, blocking: bool) -> std::io::Result<bool> {
        let op = if blocking { LOCK_EX } else { LOCK_EX | LOCK_NB };
        let r = unsafe { flock(f.as_raw_fd(), op) };
        if r == 0 {
            Ok(true)
        } else if blocking {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(false)
        }
    }

    pub fn unlock(f: &File) {
        unsafe {
            flock(f.as_raw_fd(), LOCK_UN);
        }
    }
}

/// Owns the File while the lock is held.
///
/// Holding only a `&File` made the borrow checker refuse every later `&mut`
/// read/write on the same handle -- correctly, since the guard outlives them.
/// Owning it and handing back a `&mut` through DerefMut keeps one handle, one
/// owner, and an unlock that cannot be skipped.
pub struct FileLock {
    f: Option<File>,
}

impl FileLock {
    pub fn acquire(f: File) -> std::io::Result<FileLock> {
        sys::lock(&f, true)?;
        Ok(FileLock { f: Some(f) })
    }

    pub fn file_mut(&mut self) -> &mut File {
        self.f.as_mut().expect("lock already released")
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        if let Some(f) = self.f.take() {
            sys::unlock(&f);
        }
    }
}

/// Refuses to let a second collector run on this machine.
///
/// Two collectors at once doubled the connection rate and got this host
/// suspended; a rate gate alone would hold the total in check now, but two
/// collectors also split one budget so neither completes a sweep.
pub struct SingleInstance {
    file: Option<File>,
    owner_path: PathBuf,
}

impl SingleInstance {
    pub fn acquire(name: &str) -> Result<SingleInstance, String> {
        let dir = state_dir();
        let _ = fs::create_dir_all(&dir);
        let lock_path = dir.join(format!("{}.lock", name));
        let owner_path = dir.join(format!("{}.owner", name));

        let f = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(&lock_path)
            .map_err(|e| e.to_string())?;

        match sys::lock(&f, false) {
            Ok(true) => {}
            _ => {
                let who = fs::read_to_string(&owner_path)
                    .unwrap_or_else(|_| "unknown".into());
                return Err(who.trim().to_string());
            }
        }
        let _ = fs::write(
            &owner_path,
            format!("pid {}, rust collector", std::process::id()),
        );
        Ok(SingleInstance {
            file: Some(f),
            owner_path,
        })
    }
}

impl Drop for SingleInstance {
    fn drop(&mut self) {
        if let Some(f) = self.file.take() {
            sys::unlock(&f);
        }
        let _ = fs::remove_file(&self.owner_path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamps_to_hard_ceiling() {
        let g = Gate::new(0.0, 1.0, 10_000);
        assert!(g.min_gap >= 1.0 / HARD_CEILING_PER_SEC);
        assert!(g.rate_per_sec() <= HARD_CEILING_PER_SEC + 1e-9);
        assert!(g.rate_per_sec() * 20.0 <= 60.0, "must stay far under the campus limit");
    }

    #[test]
    fn peak_counts_a_one_second_window() {
        assert_eq!(peak_per_sec(vec![0.0, 0.1, 0.2, 5.0]), 3);
        assert_eq!(peak_per_sec(vec![0.0, 2.0, 4.0]), 1);
        assert_eq!(peak_per_sec(vec![]), 0);
    }

    #[test]
    fn stamps_round_trip() {
        let v = vec![1.5, 2.25, 3.0];
        assert_eq!(parse_stamps(&render_stamps(&v)), v);
        // Must also read what the Python side writes.
        assert_eq!(parse_stamps("[1.5, 2.25, 3.0]"), v);
        assert_eq!(parse_stamps(""), Vec::<f64>::new());
    }
}
