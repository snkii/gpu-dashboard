// Copyright (c) 2026 Seonuk Kim, Human Interface Laboratory, Seoul National University
// SPDX-License-Identifier: MIT
//! HIL GPU monitor.
//!
//! One persistent SSH stream per server, a rate gate shared with every other
//! process on this machine, and a dashboard that never touches SSH on the
//! request path.
//!
//!     hilmon                 collect + serve on 127.0.0.1:8899
//!     hilmon --check         connect, print what each server reports, exit
//!     hilmon --once          one snapshot as JSON, exit
//!     hilmon --publish DIR   write status.json + the page for a static host
//!     hilmon --rate          show the SSH rate ceiling and recent history

mod collect;
mod db;
mod gate;
mod http;
mod json;
mod model;
mod power;

use collect::Collector;
use model::{gpu_is_busy, Config, Status};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

struct Args {
    config: String,
    port: u16,
    bind: String,
    web: PathBuf,
    publish: Option<String>,
    upload_cmd: Option<String>,
    upload_cmd_tick: Option<String>,
    upload_cmd_stats: Option<String>,
    stats_interval: u64,
    interval: u64,
    check: bool,
    once: bool,
    rate: bool,
    loop_: bool,
}

fn parse_args() -> Result<Args, String> {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|x| x.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."));
    // Run from the repo during development, from beside the binary once
    // installed; prefer the working directory when it looks like the project.
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let base = if cwd.join("servers.json").exists() { cwd } else { exe_dir };

    let mut a = Args {
        config: base.join("servers.json").to_string_lossy().into_owned(),
        port: 8899,
        bind: "127.0.0.1".into(),
        web: base.join("web"),
        publish: None,
        upload_cmd: None,
        upload_cmd_tick: None,
        upload_cmd_stats: None,
        stats_interval: 60,
        interval: 2,
        check: false,
        once: false,
        rate: false,
        loop_: false,
    };

    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < argv.len() {
        let k = argv[i].clone();
        let mut take = |i: &mut usize| -> Result<String, String> {
            *i += 1;
            argv.get(*i)
                .cloned()
                .ok_or_else(|| format!("{} needs a value", k))
        };
        match argv[i].as_str() {
            "-c" | "--config" => a.config = take(&mut i)?,
            "-p" | "--port" => {
                a.port = take(&mut i)?.parse().map_err(|_| "bad port".to_string())?
            }
            "--bind" => a.bind = take(&mut i)?,
            "--web" => a.web = PathBuf::from(take(&mut i)?),
            "--publish" => a.publish = Some(take(&mut i)?),
            "--upload-cmd" => a.upload_cmd = Some(take(&mut i)?),
            "--upload-cmd-tick" => a.upload_cmd_tick = Some(take(&mut i)?),
            "--upload-cmd-stats" => a.upload_cmd_stats = Some(take(&mut i)?),
            "--stats-interval" => {
                a.stats_interval = take(&mut i)?.parse().map_err(|_| "bad --stats-interval")?;
            }
            "--publish-interval" => {
                a.interval = take(&mut i)?
                    .parse()
                    .map_err(|_| "bad interval".to_string())?
            }
            "--check" => a.check = true,
            "--once" => a.once = true,
            "--rate" => a.rate = true,
            "--loop" => a.loop_ = true,
            "-h" | "--help" => {
                print_help();
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument {:?}", other)),
        }
        i += 1;
    }
    Ok(a)
}

fn print_help() {
    println!(
        "hilmon - HIL GPU monitor\n\n\
         hilmon                        collect + serve on 127.0.0.1:8899\n\
         hilmon --check                connect, report each server, exit\n\
         hilmon --once                 one snapshot as JSON, exit\n\
         hilmon --publish DIR [--loop] write status.json + page for upload\n\
         hilmon --rate                 show the SSH rate ceiling and history\n\n\
         --config PATH            servers.json (default: beside the binary)\n\
         --port N                 listen port (default 8899)\n\
         --bind ADDR              listen address (default 127.0.0.1)\n\
         --web DIR                directory holding index.html and icons/\n\
         --publish-interval SEC   upload cadence with --loop (default 2)\n\
         --upload-cmd CMD         run in DIR after the FIRST publish\n\
         --upload-cmd-tick CMD    run in DIR on every later tick
\
         --upload-cmd-stats CMD   run in DIR after each stats.json rewrite
\
         --stats-interval SEC     stats.json cadence (default 60)"
    );
}

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("error: {}", e);
            print_help();
            std::process::exit(2);
        }
    };

    let cfg = match Config::load(&args.config) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {}", e);
            std::process::exit(1);
        }
    };

    if args.rate {
        show_rate(&cfg);
        return;
    }

    let public = cfg.public_mode;
    let n = cfg.active().len();
    let g = gate::Gate::new(cfg.min_gap, cfg.window, cfg.max_per_window);

    // One collector per machine. Two would split a single rate budget so
    // neither keeps up -- and two collectors at once is what got this
    // machine's network suspended.
    let _guard = match gate::SingleInstance::acquire("collector") {
        Ok(g) => g,
        Err(who) => {
            // Exit 0, not 1: "exactly one collector is running" is the
            // invariant this lock exists to keep, so finding it already held
            // means the system is healthy. A watchdog task can therefore fire
            // every few minutes and do nothing, without the scheduler
            // reporting a permanent failure.
            println!("a collector is already running ({}); nothing to do.", who);
            return;
        }
    };

    let mut c = Collector::new(cfg);
    c.start();
    let c = Arc::new(c);

    if args.check {
        run_check(&c, n);
        return;
    }

    if let Some(dir) = args.publish.clone() {
        run_publish(&c, &args, &dir, public);
        return;
    }

    if args.once {
        wait_for_first(&c, n, 45);
        println!("{}", c.snapshot_json(public));
        return;
    }

    let addr = format!("{}:{}", args.bind, args.port);
    println!("HIL GPU monitor  ->  http://{}", addr);
    println!("  {} servers, one persistent SSH stream each, data every 1s", n);
    println!(
        "  connections open at most {:.2}/sec and are never re-opened while healthy",
        g.rate_per_sec()
    );
    println!("  Ctrl+C to stop");

    let srv = http::Server::new(Arc::clone(&c), args.web.clone(), public);
    if let Err(e) = srv.serve(&addr) {
        eprintln!("listen failed: {}", e);
        std::process::exit(1);
    }
}

fn show_rate(cfg: &Config) {
    let g = gate::Gate::new(cfg.min_gap, cfg.window, cfg.max_per_window);
    let hist = gate::history();
    let t = gate::now();
    let within = |w: f64| hist.iter().filter(|&&x| t - x < w).count();
    println!("SSH rate gate");
    println!(
        "  file             : {}",
        gate::state_dir().join("ssh-gate.json").display()
    );
    println!("  min gap          : {:.2}s between connections", g.min_gap);
    println!(
        "  window           : {} connections / {:.0}s",
        g.max_per_window, g.window
    );
    println!("  sustained ceiling: {:.2} conn/sec", g.rate_per_sec());
    println!(
        "  hard ceiling     : {:.2} conn/sec (not configurable higher)",
        gate::HARD_CEILING_PER_SEC
    );
    println!("  campus limit     : ~60 conn/sec");
    println!(
        "  margin           : {:.0}x below the campus limit",
        60.0 / g.rate_per_sec().max(1e-9)
    );
    println!("recent history");
    println!(
        "  last 1s / 10s / 60s : {} / {} / {}",
        within(1.0),
        within(10.0),
        within(60.0)
    );
    println!("  peak in any 1s      : {}", gate::peak_per_sec(hist));
}

/// Wait for the streams to come up, but stop as soon as they stop arriving.
///
/// Waiting for ALL of them never finishes when a server is powered off or
/// fail2ban'd: hi30 has been unreachable for hours. Give up once no new server
/// has connected for a while and report what did.
fn wait_for_first(c: &Collector, n: usize, secs: u64) {
    let stall_limit = 12;
    let mut best = 0usize;
    let mut stalled = 0u64;
    for _ in 0..secs {
        let up = c
            .samples()
            .values()
            .filter(|x| x.status == Status::Ok)
            .count();
        if up >= n {
            return;
        }
        if up > best {
            best = up;
            stalled = 0;
        } else {
            stalled += 1;
            if best > 0 && stalled >= stall_limit {
                return;
            }
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

fn run_check(c: &Collector, n: usize) {
    println!(
        "connecting to {} servers (the gate opens them one at a time)...\n",
        n
    );
    wait_for_first(c, n, 90);
    let samples = c.samples();
    let cfg = c.config();
    let mut up = 0;
    let mut gpus = 0;
    for s in cfg.active() {
        let smp = match samples.get(&s.name) {
            Some(x) => x,
            None => continue,
        };
        if smp.status == Status::Ok {
            up += 1;
            gpus += smp.gpus.len();
            let busy = smp.gpus.iter().filter(|g| gpu_is_busy(g)).count();
            let watts: f64 = smp.gpus.iter().filter_map(|g| g.power).sum();
            println!(
                "  OK   {:<5} {:<18} {} GPU  {}/{} busy  {:.0} W",
                s.name,
                s.host,
                smp.gpus.len(),
                busy,
                smp.gpus.len(),
                watts
            );
        } else {
            println!("  FAIL {:<5} {:<18} {}", s.name, s.host, smp.error);
        }
    }
    println!("\n{}/{} up, {} GPUs visible", up, n, gpus);
}

/// HH:MM:SS in local time, for the log. Every publisher line carries one: a
/// log without times cannot answer "when did it stop", which is the only
/// question anyone asks of it.
fn stamp() -> String {
    let secs = gate::now() as i64 + local_offset();
    let d = secs.rem_euclid(86_400);
    format!("{:02}:{:02}:{:02}", d / 3600, (d % 3600) / 60, d % 60)
}

/// Seconds east of UTC. Read once from the OS rather than assumed, and cached
/// because it cannot change often enough to matter here.
fn local_offset() -> i64 {
    use std::sync::OnceLock;
    static OFF: OnceLock<i64> = OnceLock::new();
    *OFF.get_or_init(|| {
        // `tzutil /g` needs parsing; asking PowerShell for the current offset is
        // one call and needs none. A failure just means UTC timestamps.
        std::process::Command::new("powershell")
            .args([
                "-NoProfile",
                "-Command",
                "[int][datetime]::Now.Subtract([datetime]::UtcNow).TotalMinutes",
            ])
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .and_then(|s| s.trim().parse::<i64>().ok())
            .map(|m| m * 60)
            .unwrap_or(0)
    })
}

fn write_atomic(dir: &str, name: &str, body: &[u8]) -> std::io::Result<()> {
    // Write to a temp name and rename: a concurrent uploader must never read a
    // half-written file.
    let tmp = PathBuf::from(dir).join(format!("{}.tmp", name));
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, PathBuf::from(dir).join(name))
}

fn run_publish(c: &Arc<Collector>, args: &Args, dir: &str, public: bool) {
    let _ = std::fs::create_dir_all(dir);
    let n = c.config().active().len();
    println!(
        "{} START streaming: {} servers, one persistent connection each, publishing every {}s",
        stamp(),
        n,
        args.interval
    );
    wait_for_first(c, n, 45);

    let mut first = true;
    let mut last_stats = 0.0f64;
    let mut last_prune = gate::now();
    loop {
        let t0 = std::time::Instant::now();
        let body = c.snapshot_json(public);

        if let Err(e) = write_atomic(dir, "status.json", body.as_bytes()) {
            eprintln!("publish error: {}", e);
        }

        if first {
            copy_web(&args.web, dir);
        }

        // Statistics are a separate, much larger file on a slower cadence, and
        // the page only fetches it when the 통계 tab is opened.
        let now = gate::now();
        if first || now - last_stats >= args.stats_interval as f64 {
            last_stats = now;
            let sj = c.stats_json(&c.stats_ranges(now as u32), now as u32);
            let kb = sj.len() / 1024;
            match write_atomic(dir, "stats.json", sj.as_bytes()) {
                Ok(()) => {
                    let mut m = format!("{}   stats.json {} KB", stamp(), kb);
                    // Not on the first tick: the initial upload command syncs
                    // the whole directory, and it runs below.
                    if let Some(cmd) = args.upload_cmd_stats.clone().filter(|_| !first) {
                        match run_shell(&cmd, dir) {
                            Ok(true) => m.push_str(" | upload ok"),
                            Ok(false) => m.push_str(" | upload FAILED"),
                            Err(e) => m.push_str(&format!(" | upload error {}", e)),
                        }
                    }
                    println!("{}", m);
                }
                Err(e) => eprintln!("stats publish error: {}", e),
            }
        }

        // Retention, once a day. Deleting whole day files costs nothing.
        if now - last_prune >= 86_400.0 {
            last_prune = now;
            let gone = c.prune_db(c.config().stats_keep_days);
            if gone > 0 {
                println!("  stats: pruned {} old day files", gone);
            }
        }

        let samples = c.samples();
        let up = samples.values().filter(|s| s.status == Status::Ok).count();
        let (mut tot, mut busy) = (0usize, 0usize);
        for s in samples.values().filter(|s| s.status == Status::Ok) {
            for g in &s.gpus {
                tot += 1;
                if gpu_is_busy(g) {
                    busy += 1;
                }
            }
        }

        let mut msg = format!(
            "{} published {}/{} up, {}/{} GPUs free{}",
            stamp(),
            up,
            n,
            tot - busy,
            tot,
            if public { ", redacted" } else { "" }
        );

        // Only status.json changes after the first publish; syncing the whole
        // directory every tick bills eleven operations for one changed file.
        let cmd = if first {
            args.upload_cmd.clone()
        } else {
            args.upload_cmd_tick
                .clone()
                .or_else(|| args.upload_cmd.clone())
        };
        if let Some(cmd) = cmd {
            match run_shell(&cmd, dir) {
                Ok(true) => msg.push_str(" | upload ok"),
                Ok(false) => msg.push_str(" | upload FAILED"),
                Err(e) => msg.push_str(&format!(" | upload error {}", e)),
            }
        }
        println!("{}", msg);
        use std::io::Write;
        let _ = std::io::stdout().flush();

        first = false;
        if !args.loop_ {
            return;
        }
        let spent = t0.elapsed().as_secs_f64();
        std::thread::sleep(Duration::from_secs_f64(
            (args.interval as f64 - spent).max(0.5),
        ));
    }
}

fn run_shell(cmd: &str, cwd: &str) -> std::io::Result<bool> {
    let mut c = if cfg!(windows) {
        let mut c = std::process::Command::new("cmd");
        c.arg("/c").arg(cmd);
        c
    } else {
        let mut c = std::process::Command::new("sh");
        c.arg("-c").arg(cmd);
        c
    };
    let out = c.current_dir(cwd).output()?;
    if !out.status.success() {
        let e = String::from_utf8_lossy(&out.stderr);
        if !e.trim().is_empty() {
            eprintln!("  upload stderr: {}", e.trim());
        }
    }
    Ok(out.status.success())
}

fn copy_web(web: &PathBuf, dir: &str) {
    let dst = PathBuf::from(dir);
    let _ = std::fs::copy(web.join("index.html"), dst.join("index.html"));
    let icons_src = web.join("icons");
    let icons_dst = dst.join("icons");
    if icons_src.is_dir() {
        let _ = std::fs::create_dir_all(&icons_dst);
        if let Ok(rd) = std::fs::read_dir(&icons_src) {
            for e in rd.flatten() {
                if e.path().is_file() {
                    let _ = std::fs::copy(e.path(), icons_dst.join(e.file_name()));
                }
            }
        }
    }
}
