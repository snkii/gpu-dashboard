// Copyright (c) 2026 Seonuk Kim, Human Interface Laboratory, Seoul National University
// SPDX-License-Identifier: MIT
//! Config, sample types, and the snapshot the dashboard consumes.

use crate::json::{self, Value, Writer};
use std::collections::BTreeMap;

#[derive(Debug, Clone)]
pub struct Server {
    pub name: String,
    pub host: String,
    pub port: u16,
    pub gpu_model: String,
    pub n_gpu: usize,
    pub loc: String,
    pub identity: Option<String>,
    pub enabled: bool,
    pub pin_address: bool,
    pub note: String,
}

#[derive(Debug, Clone)]
pub struct Rack {
    pub room: String,
    pub short: String,
    pub slots: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub ssh_user: String,
    pub public_mode: bool,
    pub min_gap: f64,
    pub window: f64,
    pub max_per_window: usize,
    pub connect_timeout: u64,
    pub power_points: usize,
    pub power_log: bool,
    pub stats_period: u32,
    pub stats_keep_days: i64,
    pub servers: Vec<Server>,
    pub racks: Vec<Rack>,
}

impl Config {
    pub fn load(path: &str) -> Result<Config, String> {
        let raw = std::fs::read_to_string(path).map_err(|e| format!("{}: {}", path, e))?;
        let v = json::parse(&raw)?;

        let mut servers = Vec::new();
        for s in v.get("servers").and_then(|x| x.as_arr()).unwrap_or(&vec![]) {
            let name = s.str_or("name", "").to_string();
            let host = s.str_or("host", "").to_string();
            let pin = s.bool_or("pin_address", false);

            // Same guard as the Python loader: hi23 has no DNS record at all,
            // so a hostname there means someone "tidied up" the config and
            // silently took the host offline. Fail loudly instead.
            if pin && !host.chars().all(|c| c.is_ascii_digit() || c == '.' || c == ':') {
                return Err(format!(
                    "{}: host {:?} is pinned to a literal IP but looks like a hostname.\n  {}",
                    name,
                    host,
                    s.str_or("note", "")
                ));
            }

            servers.push(Server {
                name,
                host,
                port: s.num_or("port", 22.0) as u16,
                gpu_model: s.str_or("gpu", "").to_string(),
                n_gpu: s.num_or("n_gpu", 0.0) as usize,
                loc: s.str_or("loc", "").to_string(),
                identity: s.get("identity").and_then(|x| x.as_str()).map(String::from),
                enabled: s.bool_or("enabled", true),
                pin_address: pin,
                note: s.str_or("note", "").to_string(),
            });
        }

        let mut racks = Vec::new();
        for r in v.get("racks").and_then(|x| x.as_arr()).unwrap_or(&vec![]) {
            racks.push(Rack {
                room: r.str_or("room", "").to_string(),
                short: r.str_or("short", "").to_string(),
                slots: r
                    .get("slots")
                    .and_then(|x| x.as_arr())
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default(),
            });
        }

        Ok(Config {
            ssh_user: v.str_or("ssh_user", "sukim").to_string(),
            public_mode: v.bool_or("public_mode", true),
            min_gap: v.num_or("min_ssh_interval_sec", crate::gate::DEFAULT_MIN_GAP),
            window: v.num_or("ssh_rate_window_sec", crate::gate::DEFAULT_WINDOW),
            max_per_window: v.num_or(
                "ssh_max_per_window",
                crate::gate::DEFAULT_MAX_PER_WINDOW as f64,
            ) as usize,
            connect_timeout: v.num_or("ssh_connect_timeout", 6.0) as u64,
            power_points: v.num_or("power_history_points", 120.0) as usize,
            power_log: v.bool_or("power_log", true),
            // 10s rows: the statistics view cannot resolve finer, and
            // 1 Hz would be 1.4 MB per server per day.
            stats_period: v.num_or("stats_period_sec", 10.0) as u32,
            stats_keep_days: v.num_or("stats_keep_days", 365.0) as i64,
            servers,
            racks,
        })
    }

    pub fn active(&self) -> Vec<&Server> {
        let mut v: Vec<&Server> = self.servers.iter().filter(|s| s.enabled).collect();
        v.sort_by_key(|s| sort_key(&s.name));
        v
    }
}

/// hi9 must sort before hi14, so compare the numeric part first.
pub fn sort_key(name: &str) -> (u32, String) {
    let digits: String = name.chars().filter(|c| c.is_ascii_digit()).collect();
    (digits.parse().unwrap_or(0), name.to_string())
}

#[derive(Debug, Clone, Default)]
pub struct Gpu {
    pub index: u32,
    pub name: String,
    pub util: Option<f64>,
    pub mem_used: Option<f64>,
    pub mem_total: Option<f64>,
    pub power: Option<f64>,
    pub power_limit: Option<f64>,
    pub temp: Option<f64>,
    pub fan: Option<f64>,
    pub procs: Vec<f64>, // per-process memory only; identity never leaves the host
}

#[derive(Debug, Clone, Default)]
pub struct Host {
    pub load1: Option<f64>,
    pub nproc: u32,
    pub ram_total_mb: Option<f64>,
    pub ram_used_mb: Option<f64>,
    pub disk_total_kb: Option<f64>,
    pub disk_used_kb: Option<f64>,
    pub uptime_sec: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    Ok,
    Pending,
    Down,
    NoGpu,
}

impl Status {
    pub fn as_str(&self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::Pending => "pending",
            Status::Down => "down",
            Status::NoGpu => "nogpu",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Sample {
    pub status: Status,
    pub error: String,
    pub ts: u64,
    pub gpus: Vec<Gpu>,
    pub host: Host,
    pub watts: Option<f64>,
    pub watt_history: Vec<f64>,
}

impl Default for Sample {
    fn default() -> Self {
        Sample {
            status: Status::Pending,
            error: "연결 중".into(),
            ts: 0,
            gpus: Vec::new(),
            host: Host::default(),
            watts: None,
            watt_history: Vec::new(),
        }
    }
}

pub fn gpu_is_busy(g: &Gpu) -> bool {
    g.util.unwrap_or(0.0) >= 5.0 || g.mem_used.unwrap_or(0.0) > 512.0
}

/// Serialise the snapshot exactly as the dashboard expects.
///
/// `public` drops everything that identifies the network or a person. Unlike a
/// front-end auth check, this cannot be bypassed: the bytes never exist.
pub fn write_snapshot(
    cfg: &Config,
    order: &[&Server],
    samples: &BTreeMap<String, Sample>,
    public: bool,
) -> String {
    let mut total = 0usize;
    let mut busy = 0usize;
    let mut up = 0usize;
    let mut oldest: Option<u64> = None;

    for s in order {
        if let Some(smp) = samples.get(&s.name) {
            if smp.status == Status::Ok {
                up += 1;
                for g in &smp.gpus {
                    total += 1;
                    if gpu_is_busy(g) {
                        busy += 1;
                    }
                }
            }
            if smp.ts > 0 {
                oldest = Some(oldest.map_or(smp.ts, |o: u64| o.min(smp.ts)));
            }
        }
    }

    let mut w = Writer::new();
    w.raw("{");
    w.key("generated_at");
    w.num(oldest.unwrap_or(0) as f64);
    w.raw(",");
    w.key("poll_interval_sec");
    w.num(1.0);

    w.raw(",");
    w.key("racks");
    w.raw("[");
    for (i, r) in cfg.racks.iter().enumerate() {
        if i > 0 {
            w.raw(",");
        }
        w.raw("{");
        w.key("room");
        w.str(&r.room);
        w.raw(",");
        w.key("short");
        w.str(&r.short);
        w.raw(",");
        w.key("slots");
        w.raw("[");
        for (j, s) in r.slots.iter().enumerate() {
            if j > 0 {
                w.raw(",");
            }
            w.str(s);
        }
        w.raw("]}");
    }
    w.raw("]");

    w.raw(",");
    w.key("summary");
    w.raw("{");
    w.key("servers_total");
    w.num(order.len() as f64);
    w.raw(",");
    w.key("servers_up");
    w.num(up as f64);
    w.raw(",");
    w.key("gpus_total");
    w.num(total as f64);
    w.raw(",");
    w.key("gpus_busy");
    w.num(busy as f64);
    w.raw(",");
    w.key("gpus_free");
    w.num((total - busy) as f64);
    w.raw("}");

    w.raw(",");
    w.key("servers");
    w.raw("[");
    for (i, s) in order.iter().enumerate() {
        if i > 0 {
            w.raw(",");
        }
        let d = Sample::default();
        let smp = samples.get(&s.name).unwrap_or(&d);
        write_server(&mut w, s, smp, public);
    }
    w.raw("]");

    if public {
        w.raw(",");
        w.key("redacted");
        w.raw("true");
    }
    w.raw("}");
    w.buf
}

fn write_server(w: &mut Writer, s: &Server, smp: &Sample, public: bool) {
    w.raw("{");
    w.key("name");
    w.str(&s.name);
    w.raw(",");
    w.key("gpu_model");
    w.str(&s.gpu_model);
    w.raw(",");
    w.key("n_gpu_expected");
    w.num(s.n_gpu as f64);
    w.raw(",");
    w.key("loc");
    w.str(&s.loc);
    w.raw(",");
    w.key("ts");
    w.num(smp.ts as f64);
    w.raw(",");
    w.key("status");
    w.str(smp.status.as_str());

    if !public {
        // Internal mode keeps the address and the raw error, which is what
        // --check needs to be diagnosable.
        w.raw(",");
        w.key("host");
        w.str(&s.host);
        w.raw(",");
        w.key("port");
        w.num(s.port as f64);
        if !s.note.is_empty() {
            w.raw(",");
            w.key("note");
            w.str(&s.note);
        }
    }

    if smp.status != Status::Ok {
        w.raw(",");
        w.key("error");
        if public {
            // An SSH error string echoes the host and port back, so the public
            // payload gets a fixed phrase per status instead.
            w.str(match smp.status {
                Status::Pending => "연결 중",
                Status::NoGpu => "no GPU reported",
                _ => "unreachable",
            });
        } else {
            w.str(&smp.error);
        }
    }

    if let Some(v) = smp.host.load1 {
        w.raw(",");
        w.key("load1");
        w.num(v);
    }
    if smp.host.nproc > 0 {
        w.raw(",");
        w.key("nproc");
        w.num(smp.host.nproc as f64);
    }
    for (k, v) in [
        ("ram_total_mb", smp.host.ram_total_mb),
        ("ram_used_mb", smp.host.ram_used_mb),
        ("disk_total_kb", smp.host.disk_total_kb),
        ("disk_used_kb", smp.host.disk_used_kb),
        ("uptime_sec", smp.host.uptime_sec),
    ] {
        if let Some(x) = v {
            w.raw(",");
            w.key(k);
            w.num(x);
        }
    }

    if let Some(v) = smp.watts {
        w.raw(",");
        w.key("watts");
        w.num(v);
    }
    if !smp.watt_history.is_empty() {
        w.raw(",");
        w.key("watt_history");
        w.raw("[");
        for (i, v) in smp.watt_history.iter().enumerate() {
            if i > 0 {
                w.raw(",");
            }
            w.num(v.round());
        }
        w.raw("]");
    }

    if !smp.gpus.is_empty() {
        w.raw(",");
        w.key("gpus");
        w.raw("[");
        for (i, g) in smp.gpus.iter().enumerate() {
            if i > 0 {
                w.raw(",");
            }
            w.raw("{");
            w.key("index");
            w.num(g.index as f64);
            w.raw(",");
            w.key("name");
            w.str(&g.name);
            w.raw(",");
            w.key("util");
            w.opt_num(g.util);
            w.raw(",");
            w.key("mem_used");
            w.opt_num(g.mem_used);
            w.raw(",");
            w.key("mem_total");
            w.opt_num(g.mem_total);
            w.raw(",");
            w.key("power");
            w.opt_num(g.power);
            w.raw(",");
            w.key("power_limit");
            w.opt_num(g.power_limit);
            w.raw(",");
            w.key("temp");
            w.opt_num(g.temp);
            w.raw(",");
            w.key("fan");
            w.opt_num(g.fan);
            w.raw(",");
            w.key("procs");
            w.raw("[");
            for (j, m) in g.procs.iter().enumerate() {
                if j > 0 {
                    w.raw(",");
                }
                w.raw("{");
                w.key("mem");
                w.num(*m);
                w.raw("}");
            }
            w.raw("]}");
        }
        w.raw("]");
    }
    w.raw("}");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn srv(name: &str) -> Server {
        Server {
            name: name.into(),
            host: "10.0.0.1".into(),
            port: 22,
            gpu_model: "3090 24GB".into(),
            n_gpu: 1,
            loc: "301-207".into(),
            identity: None,
            enabled: true,
            pin_address: false,
            note: "출입문 비번 1234".into(),
        }
    }

    fn ok_sample() -> Sample {
        Sample {
            status: Status::Ok,
            error: String::new(),
            ts: 1_700_000_000,
            gpus: vec![Gpu {
                index: 0,
                name: "NVIDIA GeForce RTX 3090".into(),
                util: Some(97.0),
                mem_used: Some(21000.0),
                mem_total: Some(24576.0),
                power: Some(249.5),
                power_limit: Some(250.0),
                temp: Some(75.0),
                fan: Some(83.0),
                procs: vec![21000.0],
            }],
            host: Host { nproc: 48, ..Default::default() },
            watts: Some(249.5),
            watt_history: vec![100.0, 249.5],
        }
    }

    #[test]
    fn public_payload_leaks_nothing() {
        let cfg = Config {
            ssh_user: "sukim".into(), public_mode: true,
            min_gap: 2.5, window: 10.0, max_per_window: 5, connect_timeout: 6,
            power_points: 120, power_log: false, stats_period: 10, stats_keep_days: 365,
            servers: vec![srv("hi14")], racks: vec![],
        };
        let order = cfg.active();
        let mut m = BTreeMap::new();
        m.insert("hi14".to_string(), ok_sample());
        let out = write_snapshot(&cfg, &order, &m, true);

        for needle in ["10.0.0.1", "\"host\"", "\"port\"", "\"note\"", "1234", "sukim"] {
            assert!(!out.contains(needle), "leaked {:?} in {}", needle, out);
        }
        assert!(out.contains("\"redacted\":true"));
        assert!(out.contains("\"util\":97"));
        assert!(out.contains("\"watts\":249.5"));
    }

    #[test]
    fn internal_payload_keeps_diagnostics() {
        let cfg = Config {
            ssh_user: "sukim".into(), public_mode: false,
            min_gap: 2.5, window: 10.0, max_per_window: 5, connect_timeout: 6,
            power_points: 120, power_log: false, stats_period: 10, stats_keep_days: 365,
            servers: vec![srv("hi14")], racks: vec![],
        };
        let order = cfg.active();
        let mut m = BTreeMap::new();
        let mut s = ok_sample();
        s.status = Status::Down;
        s.error = "ssh: connect to host 10.0.0.1 port 22: refused".into();
        m.insert("hi14".to_string(), s);
        let out = write_snapshot(&cfg, &order, &m, false);
        assert!(out.contains("10.0.0.1"));
        assert!(out.contains("refused"));
    }

    #[test]
    fn sorts_numerically_not_lexically() {
        let mut names = vec!["hi30", "hi9", "hi14", "hi2"];
        names.sort_by_key(|n| sort_key(n));
        assert_eq!(names, vec!["hi2", "hi9", "hi14", "hi30"]);
    }

    #[test]
    fn busy_matches_the_python_rule() {
        let idle = Gpu { util: Some(0.0), mem_used: Some(4.0), ..Default::default() };
        let warm = Gpu { util: Some(0.0), mem_used: Some(10_000.0), ..Default::default() };
        let hot = Gpu { util: Some(97.0), mem_used: Some(4.0), ..Default::default() };
        assert!(!gpu_is_busy(&idle));
        assert!(gpu_is_busy(&warm), "memory alone counts as busy");
        assert!(gpu_is_busy(&hot));
    }
}
