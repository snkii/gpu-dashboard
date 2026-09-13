// Copyright (c) 2026 Seonuk Kim, Human Interface Laboratory, Seoul National University
// SPDX-License-Identifier: MIT
//! Persistent-stream collector.
//!
//! One SSH connection per server, held open. The remote forced command emits a
//! block per second, so 1-second data costs ZERO connections after the first.
//! That is the whole reason this design exists: the campus network limits how
//! often a host may OPEN a connection, not how much data flows through one.

use crate::gate::{self, Gate};
use crate::model::*;
use crate::db::{self, Db, Row};
use crate::power::PowerLog;
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

pub fn ssh_argv(cfg: &Config, s: &Server) -> Vec<String> {
    let mut v = vec![
        "-p".into(),
        s.port.to_string(),
        "-o".into(),
        "BatchMode=yes".into(),
        "-o".into(),
        "StrictHostKeyChecking=accept-new".into(),
        "-o".into(),
        format!("ConnectTimeout={}", cfg.connect_timeout),
        // Without these a half-dead TCP session can hang for many minutes
        // before the OS gives up, and the stream looks alive while delivering
        // nothing.
        "-o".into(),
        "ServerAliveInterval=15".into(),
        "-o".into(),
        "ServerAliveCountMax=3".into(),
    ];
    // Pin the restricted monitoring key. Falling back to ~/.ssh/config would
    // offer a human's unrestricted login key instead, throwing away the point
    // of the forced-command key.
    if let Some(id) = &s.identity {
        v.push("-i".into());
        v.push(expand_home(id));
        v.push("-o".into());
        v.push("IdentitiesOnly=yes".into());
    }
    v.push(format!("{}@{}", cfg.ssh_user, s.host));
    v
}

fn expand_home(p: &str) -> String {
    if let Some(rest) = p.strip_prefix("~/") {
        let home = std::env::var("USERPROFILE")
            .or_else(|_| std::env::var("HOME"))
            .unwrap_or_default();
        format!("{}/{}", home, rest)
    } else {
        p.to_string()
    }
}

/// Parse one `###`-delimited block into whatever it carried.
///
/// Fast blocks carry only GPU lines; the caller merges the last slow block's
/// host stats onto them so a card never flickers between populated and blank.
pub fn parse_block(text: &str) -> (Vec<Gpu>, Option<Host>, Option<BTreeMap<String, Vec<f64>>>) {
    let mut section = "";
    let mut gpus: Vec<Gpu> = Vec::new();
    let mut uuid_to_index: BTreeMap<String, u32> = BTreeMap::new();
    let mut procs: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    let mut host = Host::default();
    let mut saw_host = false;
    let mut saw_proc = false;

    for line in text.lines() {
        if let Some(tag) = line.strip_prefix("###") {
            section = match tag.trim() {
                "GPU" => "gpu",
                "PROC" => "proc",
                "LOAD" => "load",
                "NPROC" => "nproc",
                "MEM" => "mem",
                "DISK" => "disk",
                "UP" => "up",
                _ => "",
            };
            continue;
        }
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match section {
            "gpu" => {
                let p: Vec<&str> = line.split(',').map(|x| x.trim()).collect();
                if p.len() < 10 {
                    continue;
                }
                let idx = match p[0].parse::<u32>() {
                    Ok(i) => i,
                    Err(_) => continue,
                };
                uuid_to_index.insert(p[2].to_string(), idx);
                gpus.push(Gpu {
                    index: idx,
                    name: p[1].to_string(),
                    util: num(p[3]),
                    mem_used: num(p[4]),
                    mem_total: num(p[5]),
                    power: num(p[6]),
                    power_limit: num(p[7]),
                    temp: num(p[8]),
                    fan: num(p[9]),
                    procs: Vec::new(),
                });
            }
            "proc" => {
                saw_proc = true;
                let p: Vec<&str> = line.split(',').map(|x| x.trim()).collect();
                if p.len() < 3 {
                    continue;
                }
                if let Some(m) = num(p[2]) {
                    procs.entry(p[0].to_string()).or_default().push(m);
                }
            }
            "load" => {
                saw_host = true;
                host.load1 = line.split_whitespace().next().and_then(num);
            }
            "nproc" => {
                saw_host = true;
                host.nproc = line.parse().unwrap_or(0);
            }
            "mem" => {
                saw_host = true;
                let f: Vec<&str> = line.split_whitespace().collect();
                if f.len() > 2 {
                    host.ram_total_mb = num(f[1]);
                    host.ram_used_mb = num(f[2]);
                }
            }
            "disk" => {
                saw_host = true;
                let f: Vec<&str> = line.split_whitespace().collect();
                if f.len() > 2 {
                    host.disk_total_kb = num(f[1]);
                    host.disk_used_kb = num(f[2]);
                }
            }
            "up" => {
                saw_host = true;
                host.uptime_sec = num(line);
            }
            _ => {}
        }
    }

    gpus.sort_by_key(|g| g.index);
    // Attach process memory by GPU uuid.
    let mut by_index: BTreeMap<u32, Vec<f64>> = BTreeMap::new();
    for (uuid, mems) in &procs {
        if let Some(i) = uuid_to_index.get(uuid) {
            by_index.insert(*i, mems.clone());
        }
    }
    if saw_proc {
        for g in &mut gpus {
            g.procs = by_index.get(&g.index).cloned().unwrap_or_default();
        }
    }

    (
        gpus,
        if saw_host { Some(host) } else { None },
        if saw_proc { Some(by_index_to_map(&by_index)) } else { None },
    )
}

fn by_index_to_map(m: &BTreeMap<u32, Vec<f64>>) -> BTreeMap<String, Vec<f64>> {
    m.iter().map(|(k, v)| (k.to_string(), v.clone())).collect()
}

fn num(s: &str) -> Option<f64> {
    let s = s.trim();
    // nvidia-smi prints these for a card that does not report the value.
    if s.is_empty()
        || s == "N/A"
        || s == "[N/A]"
        || s == "[Not Supported]"
        || s == "Not Supported"
        || s == "unknown"
    {
        return None;
    }
    s.parse().ok()
}

struct Shared {
    samples: Mutex<BTreeMap<String, Sample>>,
    power: Mutex<PowerLog>,
    db: Mutex<Db>,
}

/// Columns published for each server. `gpus == 0` marks a slot with no data.
const COLS: [(&str, fn(&Row) -> f64); 6] = [
    ("w", |r| r.watts as f64),
    ("u", |r| r.util_avg as f64),
    ("x", |r| r.util_max as f64),
    ("t", |r| r.temp_max as f64),
    ("b", |r| r.busy as f64),
    ("g", |r| r.gpus as f64),
];

pub struct Collector {
    cfg: Arc<Config>,
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
    handles: Vec<thread::JoinHandle<()>>,
}

impl Collector {
    pub fn new(cfg: Config) -> Collector {
        let mut samples = BTreeMap::new();
        for s in cfg.active() {
            samples.insert(s.name.clone(), Sample::default());
        }
        let power = PowerLog::new(cfg.power_points, cfg.power_log);
        let stats = Db::open(
            std::env::current_dir()
                .unwrap_or_else(|_| std::path::PathBuf::from("."))
                .join("stats"),
            cfg.stats_period,
        );
        Collector {
            cfg: Arc::new(cfg),
            shared: Arc::new(Shared {
                samples: Mutex::new(samples),
                power: Mutex::new(power),
                db: Mutex::new(stats),
            }),
            stop: Arc::new(AtomicBool::new(false)),
            handles: Vec::new(),
        }
    }

    pub fn start(&mut self) {
        let gate = Arc::new(Gate::new(
            self.cfg.min_gap,
            self.cfg.window,
            self.cfg.max_per_window,
        ));
        for s in self.cfg.active() {
            let cfg = Arc::clone(&self.cfg);
            let shared = Arc::clone(&self.shared);
            let stop = Arc::clone(&self.stop);
            let gate = Arc::clone(&gate);
            let name = s.name.clone();
            self.handles.push(thread::spawn(move || {
                worker(cfg, shared, stop, gate, name);
            }));
        }
    }

    pub fn shutdown(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }

    pub fn snapshot_json(&self, public: bool) -> String {
        let samples = self.shared.samples.lock().unwrap();
        let order = self.cfg.active();
        write_snapshot(&self.cfg, &order, &samples, public)
    }

    pub fn samples(&self) -> BTreeMap<String, Sample> {
        self.shared.samples.lock().unwrap().clone()
    }

    pub fn config(&self) -> Arc<Config> {
        Arc::clone(&self.cfg)
    }

    /// The ranges the statistics view offers, including the all-time one.
    ///
    /// All-time is computed rather than fixed because its start is wherever the
    /// database happens to begin; on a fresh install that is today, and it
    /// lengthens on its own.
    pub fn stats_ranges(&self, now: u32) -> Vec<(&'static str, u32, usize)> {
        let mut v = db::STATS_RANGES.to_vec();
        let earliest = self.shared.db.lock().unwrap().earliest();
        // One hour is the floor: a span shorter than a single slot would make
        // an empty chart out of data that does exist.
        let span = earliest
            .map(|e| now.saturating_sub(e))
            .unwrap_or(0)
            .max(3600);
        v.push(("all", span, 200));
        v
    }

    /// Statistics for the page, as several fixed time grids.
    ///
    /// Columnar rather than an array of objects per point: the same numbers in
    /// about a third of the bytes, and the page plots columns anyway. Every
    /// server shares one slot grid per range, so the page can sum servers into
    /// racks without re-aligning timestamps.
    pub fn stats_json(&self, ranges: &[(&str, u32, usize)], now: u32) -> String {
        let d = self.shared.db.lock().unwrap();
        let active = self.cfg.active();
        let mut w = crate::json::Writer::new();
        w.raw("{");
        w.key("now");
        w.num(now as f64);
        w.raw(",");
        w.key("servers");
        w.raw("[");
        for (i, s) in active.iter().enumerate() {
            if i > 0 {
                w.raw(",");
            }
            w.raw("{");
            w.key("name");
            w.str(&s.name);
            w.raw(",");
            w.key("gpu");
            w.str(&s.gpu_model);
            w.raw(",");
            w.key("n_gpu");
            w.num(s.n_gpu as f64);
            w.raw(",");
            w.key("loc");
            w.str(&s.loc);
            w.raw("}");
        }
        w.raw("],");
        w.key("ranges");
        w.raw("{");
        for (ri, (label, span, buckets)) in ranges.iter().enumerate() {
            if ri > 0 {
                w.raw(",");
            }
            let buckets = (*buckets).max(1);
            let step = (span / buckets as u32).max(1);
            // Snap the grid to a step boundary so the slots do not shift under
            // the viewer every time the file is republished.
            let from = (now.saturating_sub(*span) / step) * step;
            w.key(label);
            w.raw("{");
            w.key("from");
            w.num(from as f64);
            w.raw(",");
            w.key("step");
            w.num(step as f64);
            w.raw(",");
            w.key("n");
            w.num(buckets as f64);
            w.raw(",");
            w.key("data");
            w.raw("{");
            for (i, s) in active.iter().enumerate() {
                if i > 0 {
                    w.raw(",");
                }
                // Eight samples per slot is far more than the average of a
                // slot needs, and it bounds the read for a range that may
                // cover a year.
                let rows = db::grid(
                    &d.range_strided(&s.name, from, now, buckets * 8),
                    from,
                    step,
                    buckets,
                );
                w.key(&s.name);
                w.raw("{");
                for (ci, col) in COLS.iter().enumerate() {
                    if ci > 0 {
                        w.raw(",");
                    }
                    w.key(col.0);
                    w.raw("[");
                    for (j, r) in rows.iter().enumerate() {
                        if j > 0 {
                            w.raw(",");
                        }
                        w.num((col.1)(r));
                    }
                    w.raw("]");
                }
                w.raw("}");
            }
            w.raw("}}");
        }
        w.raw("}}");
        w.buf
    }

    pub fn prune_db(&self, keep_days: i64) -> usize {
        let d = self.shared.db.lock().unwrap();
        d.prune(keep_days, gate::now() as i64)
    }

    pub fn db_info(&self) -> (u64, String) {
        let d = self.shared.db.lock().unwrap();
        (d.size_bytes(), d.dir().display().to_string())
    }
}

fn worker(
    cfg: Arc<Config>,
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
    gate: Arc<Gate>,
    name: String,
) {
    let mut fails = 0u32;
    while !stop.load(Ordering::SeqCst) {
        let srv = match cfg.servers.iter().find(|s| s.name == name) {
            Some(s) => s.clone(),
            None => return,
        };
        set_state(&shared, &name, Status::Pending, "connecting");

        gate.acquire();
        if stop.load(Ordering::SeqCst) {
            return;
        }

        match session(&cfg, &srv, &shared, &stop) {
            Ok(_) => fails = 0,
            Err(e) => {
                set_state(&shared, &name, Status::Down, &e);
            }
        }
        if stop.load(Ordering::SeqCst) {
            return;
        }
        fails += 1;
        // Back off so a powered-down or fail2ban'd host is not hammered --
        // retrying into a block is what extends it.
        let delay = (5u64 << fails.min(6)).min(300);
        set_state(
            &shared,
            &name,
            Status::Down,
            &format!("reconnecting in {}s", delay),
        );
        for _ in 0..delay {
            if stop.load(Ordering::SeqCst) {
                return;
            }
            thread::sleep(Duration::from_secs(1));
        }
    }
}

fn session(
    cfg: &Config,
    srv: &Server,
    shared: &Arc<Shared>,
    stop: &Arc<AtomicBool>,
) -> Result<(), String> {
    let mut ssh = Command::new("ssh");
    ssh.args(ssh_argv(cfg, srv))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null());
    crate::no_window(&mut ssh);
    let mut child: Child = ssh.spawn().map_err(|e| format!("spawn ssh: {}", e))?;

    let out = child.stdout.take().ok_or("no stdout")?;
    let reader = BufReader::new(out);
    let mut buf = String::new();
    let mut last_host: Option<Host> = None;
    let mut got_any = false;

    for line in reader.lines() {
        if stop.load(Ordering::SeqCst) {
            let _ = child.kill();
            return Ok(());
        }
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                let _ = child.kill();
                return Err(format!("read: {}", e));
            }
        };
        if line.trim() == "###END" {
            let (gpus, host, _) = parse_block(&buf);
            buf.clear();
            if let Some(h) = host {
                last_host = Some(h);
            }
            if !gpus.is_empty() {
                got_any = true;
                commit(shared, &srv.name, gpus, last_host.clone());
            }
            continue;
        }
        buf.push_str(&line);
        buf.push('\n');
        if buf.len() > 256 * 1024 {
            buf.clear(); // runaway remote output; resync on the next ###END
        }
    }

    // stdout closed: the remote command exited or the link dropped.
    let mut err = String::new();
    if let Some(mut e) = child.stderr.take() {
        use std::io::Read;
        let _ = e.read_to_string(&mut err);
    }
    let _ = child.wait();
    let msg = err
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty() && !l.contains("Pseudo-terminal"))
        .unwrap_or(if got_any { "stream ended" } else { "no data" })
        .to_string();
    Err(msg)
}

fn commit(shared: &Arc<Shared>, name: &str, gpus: Vec<Gpu>, host: Option<Host>) {
    let watts: f64 = gpus.iter().filter_map(|g| g.power).sum();
    let has_power = gpus.iter().any(|g| g.power.is_some());

    let hist = {
        let mut p = shared.power.lock().unwrap();
        if has_power {
            p.record(name, watts, &gpus);
        }
        p.series(name)
    };

    // Statistics store. Throttled inside Db::append, so calling it every
    // second is fine -- it writes one row per stats_period.
    if !gpus.is_empty() {
        let n = gpus.len() as u32;
        let util_sum: f64 = gpus.iter().filter_map(|g| g.util).sum();
        let mem_used: f64 = gpus.iter().filter_map(|g| g.mem_used).sum();
        let mem_tot: f64 = gpus.iter().filter_map(|g| g.mem_total).sum();
        let row = Row {
            ts: gate::now() as u32,
            watts: watts.round().clamp(0.0, 65535.0) as u16,
            watt_cap: gpus.iter().filter_map(|g| g.power_limit).sum::<f64>()
                .round().clamp(0.0, 65535.0) as u16,
            util_avg: (util_sum / n as f64).round().clamp(0.0, 100.0) as u8,
            util_max: gpus.iter().filter_map(|g| g.util).fold(0.0f64, f64::max)
                .round().clamp(0.0, 100.0) as u8,
            temp_max: gpus.iter().filter_map(|g| g.temp).fold(0.0f64, f64::max)
                .round().clamp(0.0, 255.0) as u8,
            gpus: n.min(255) as u8,
            busy: gpus.iter().filter(|g| crate::model::gpu_is_busy(g)).count().min(255) as u8,
            mem_pct: if mem_tot > 0.0 {
                ((mem_used / mem_tot) * 1000.0).round().clamp(0.0, 1000.0) as u16
            } else { 0 },
        };
        if let Ok(mut d) = shared.db.lock() {
            let _ = d.append(name, row);
        }
    }

    let mut s = shared.samples.lock().unwrap();
    let prev = s.get(name).cloned().unwrap_or_default();
    let mut gpus = gpus;
    // procs only arrive on slow blocks; keep the previous counts on fast ones
    // so the "N jobs" label does not blink out every second.
    if gpus.iter().all(|g| g.procs.is_empty()) {
        for g in &mut gpus {
            if let Some(old) = prev.gpus.iter().find(|o| o.index == g.index) {
                g.procs = old.procs.clone();
            }
        }
    }
    s.insert(
        name.to_string(),
        Sample {
            status: Status::Ok,
            error: String::new(),
            ts: gate::now() as u64,
            gpus,
            host: host.unwrap_or(prev.host),
            watts: if has_power { Some(watts) } else { None },
            watt_history: hist,
        },
    );
}

fn set_state(shared: &Arc<Shared>, name: &str, st: Status, err: &str) {
    let mut s = shared.samples.lock().unwrap();
    if let Some(x) = s.get_mut(name) {
        // Keep the last good numbers on screen while reconnecting; only the
        // status and the age change.
        x.status = st;
        x.error = err.to_string();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BLOCK: &str = "\
###GPU
0, NVIDIA GeForce RTX 3090, GPU-aaa, 97, 21916, 24576, 249.46, 250.00, 69, 83
1, NVIDIA GeForce RTX 3090, GPU-bbb, 0, 1, 24576, [N/A], [N/A], 46, 58
###PROC
GPU-aaa, 3584667, 21910
###LOAD
12.44 9.87 8.10 5/2411 31999
###NPROC
48
###MEM
Mem:  515000 312000 1200
###DISK
/dev/sda2 3844280832 1122334455 2721946377 30% /
###UP
1728394
";

    #[test]
    fn parses_a_full_block() {
        let (gpus, host, procs) = parse_block(BLOCK);
        assert_eq!(gpus.len(), 2);
        assert_eq!(gpus[0].util, Some(97.0));
        assert_eq!(gpus[0].power, Some(249.46));
        assert_eq!(gpus[1].power, None, "[N/A] must become None, not 0");
        assert_eq!(gpus[0].procs, vec![21910.0]);
        assert!(gpus[1].procs.is_empty());
        let h = host.expect("host stats present");
        assert_eq!(h.nproc, 48);
        assert_eq!(h.load1, Some(12.44));
        assert_eq!(h.ram_total_mb, Some(515000.0));
        assert!(procs.is_some());
    }

    #[test]
    fn fast_block_has_no_host_stats() {
        let fast = "###GPU\n0, X, GPU-a, 50, 1, 2, 10, 20, 30, 40\n";
        let (gpus, host, procs) = parse_block(fast);
        assert_eq!(gpus.len(), 1);
        assert!(host.is_none(), "caller must merge the last slow block");
        assert!(procs.is_none());
    }

    #[test]
    fn identity_is_pinned_in_argv() {
        let cfg = Config {
            ssh_user: "sukim".into(), public_mode: true,
            min_gap: 2.5, window: 10.0, max_per_window: 5, connect_timeout: 6,
            power_points: 10, power_log: false, stats_period: 10, stats_keep_days: 365,
            servers: vec![], racks: vec![],
        };
        let s = Server {
            name: "hi14".into(), host: "h".into(), port: 2222,
            gpu_model: String::new(), n_gpu: 1, loc: String::new(),
            identity: Some("~/.ssh/hil_monitor".into()),
            enabled: true, pin_address: false, note: String::new(),
        };
        let a = ssh_argv(&cfg, &s);
        assert!(a.contains(&"IdentitiesOnly=yes".to_string()));
        assert!(a.iter().any(|x| x.ends_with("hil_monitor")));
        assert!(a.contains(&"2222".to_string()));
        assert_eq!(a.last().unwrap(), "sukim@h");
    }
}
