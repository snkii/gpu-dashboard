// Copyright (c) 2026 Seonuk Kim, Human Interface Laboratory, Seoul National University
// SPDX-License-Identifier: MIT

//! A short written summary of the current state, from Gemini.
//!
//! The call happens HERE, on the collector machine, and the result is published
//! as a static file. It must never happen in the browser: a key shipped to the
//! page is a key handed to everyone who opens it, and anyone could then spend
//! the quota. This also keeps the one-way shape of the whole system -- the
//! desktop talks outward, nothing talks inward.
//!
//! Nothing is sent that is not already public. The digest is built from the
//! same redacted snapshot that is published to the web, so a request to Google
//! reveals nothing a visitor could not already read.
//!
//! HTTPS goes through `curl`, which ships with Windows. Rust's standard library
//! has no TLS, and the alternative was a dependency tree for one request every
//! few minutes -- for a program whose lack of dependencies is a feature.

use crate::json;
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

pub struct Summarizer {
    key_path: PathBuf,
    model: String,
    prompt: String,
    timeout: u32,
    /// Last successful text, so a failed call leaves the old summary standing
    /// rather than blanking the banner.
    pub text: String,
    pub ts: u32,
    pub failures: u32,
    /// Total draw and time at the last successful briefing, for the "since
    /// last time" figure. One float is the entire trend mechanism.
    last: Option<(f64, u32)>,
}

impl Summarizer {
    pub fn new(key_path: PathBuf, model: String) -> Summarizer {
        Summarizer {
            key_path,
            model,
            prompt: PROMPT.to_string(),
            timeout: 30,
            text: String::new(),
            ts: 0,
            failures: 0,
            last: None,
        }
    }

    /// Whether a key is present at all. Without one the whole feature is off,
    /// and that is a normal state, not an error.
    pub fn enabled(&self) -> bool {
        self.key_path.exists()
    }

    pub fn set_timeout(&mut self, secs: u32) {
        self.timeout = secs.max(1);
    }

    pub fn set_prompt(&mut self, p: String) {
        self.prompt = p;
    }

    pub fn read_key(&self) -> Option<String> {
        self.key()
    }

    pub fn prompt(&self) -> &str {
        &self.prompt
    }

    fn key(&self) -> Option<String> {
        let k = fs::read_to_string(&self.key_path).ok()?;
        let k = k.trim().to_string();
        if k.is_empty() {
            None
        } else {
            Some(k)
        }
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    /// Ask for a summary of the published snapshot. Returns true when the text
    /// changed.
    ///
    /// Takes the snapshot rather than the live samples so that the summary can
    /// only ever describe what a visitor can actually see.
    pub fn refresh(&mut self, snapshot: &str, now: u32) -> bool {
        let key = match self.key() {
            Some(k) => k,
            None => return false,
        };
        let trend = self
            .last
            .map(|(w, then)| (w, ((now.saturating_sub(then)) as f64 / 60.0).round() as u32));
        let facts = match digest_with_trend(snapshot, trend) {
            Some(f) => f,
            None => return false,
        };
        match self.ask_with(&key, &self.prompt, &facts) {
            Ok(t) => {
                self.failures = 0;
                let changed = t != self.text;
                self.text = t;
                self.ts = now;
                // Only after a success: a failed call did not tell anyone
                // anything, so the next one should still compare to the last
                // briefing a person actually saw.
                if let Some(w) = total_watts(snapshot) {
                    self.last = Some((w, now));
                }
                changed
            }
            Err(e) => {
                self.failures += 1;
                // Loud once, then quiet: a provider outage should not fill the
                // log with one line every few minutes for hours.
                if self.failures <= 3 {
                    eprintln!("[summary] {}", e);
                }
                false
            }
        }
    }

    /// One call, with an explicit prompt. Public so prompts can be compared
    /// against real data without running a collector.
    pub fn ask_with(&self, key: &str, prompt: &str, facts: &str) -> Result<String, String> {
        let dir = std::env::temp_dir();
        let body_path = dir.join(format!("hilmon-sum-{}.json", std::process::id()));
        let cfg_path = dir.join(format!("hilmon-sum-{}.curl", std::process::id()));

        let mut w = json::Writer::new();
        w.raw("{");
        w.key("contents");
        w.raw("[{");
        w.key("parts");
        w.raw("[{");
        w.key("text");
        w.str(&format!("{}\n\n{}", prompt, facts));
        w.raw("}]}],");
        w.key("generationConfig");
        w.raw("{");
        w.key("temperature");
        w.num(0.2);
        w.raw(",");
        // Generous on purpose. This cap covers the model's internal reasoning
        // as well as the answer, and the two share one budget: at 300 the
        // reasoning consumed all of it and the visible answer was cut off
        // after twelve characters. Turning reasoning off outright is not
        // available -- thinkingBudget: 0 is rejected with a 400 -- so the
        // budget is simply large enough that the answer cannot be squeezed
        // out. The answer itself is capped by the prompt, not by this.
        w.key("maxOutputTokens");
        w.num(2000.0);
        w.raw("}}");
        fs::write(&body_path, w.buf.as_bytes()).map_err(|e| e.to_string())?;

        let cfg = curl_config(&self.model, key, &body_path.display().to_string(), self.timeout);
        fs::write(&cfg_path, cfg.as_bytes()).map_err(|e| e.to_string())?;

        let mut cmd = Command::new("curl");
        cmd.arg("--config").arg(&cfg_path);
        crate::no_window(&mut cmd);
        let out = cmd.output();
        // Delete both before inspecting the result, so an early return cannot
        // leave the key sitting in the temp directory.
        let _ = fs::remove_file(&cfg_path);
        let _ = fs::remove_file(&body_path);

        let out = out.map_err(|e| format!("curl: {}", e))?;
        if !out.status.success() {
            return Err(format!(
                "curl exited {}: {}",
                out.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        let body = String::from_utf8_lossy(&out.stdout).into_owned();
        extract(&body)
    }
}

/// Build the curl config file.
///
/// The key lives here rather than on the command line, where any other process
/// could read it out of the argument list.
///
/// Paths are written with forward slashes because **a backslash is an escape
/// character inside a curl config file**: a Windows path written verbatim
/// arrives as `C:Userssukim...` and curl cannot open it. Windows accepts
/// forward slashes everywhere, so this costs nothing.
fn curl_config(model: &str, key: &str, body_path: &str, timeout: u32) -> String {
    format!(
        "url = \"https://generativelanguage.googleapis.com/v1beta/models/{}:generateContent\"\n\
         header = \"Content-Type: application/json\"\n\
         header = \"x-goog-api-key: {}\"\n\
         data-binary = \"@{}\"\n\
         request = \"POST\"\n\
         silent\n\
         show-error\n\
         max-time = {}\n",
        model,
        key,
        body_path.replace('\\', "/"),
        timeout
    )
}

/// Pull the answer text out of a Gemini response, or explain what came back.
fn extract(body: &str) -> Result<String, String> {
    let v = json::parse(body).map_err(|e| format!("response was not JSON: {}", e))?;
    if let Some(err) = v.get("error") {
        return Err(format!(
            "API error {}: {}",
            err.num_or("code", 0.0),
            err.str_or("message", "(no message)")
        ));
    }
    let cand = v
        .get("candidates")
        .and_then(|c| c.as_arr())
        .and_then(|a| a.first());

    // A cut-off answer is worse than no answer: it would be published to the
    // dashboard as a sentence that stops mid-word.
    if let Some(reason) = cand.and_then(|c| c.get("finishReason")).and_then(|r| r.as_str()) {
        if reason != "STOP" {
            return Err(format!("response did not finish cleanly ({})", reason));
        }
    }

    let text = cand
        .and_then(|c| c.get("content"))
        .and_then(|c| c.get("parts"))
        .and_then(|p| p.as_arr())
        .and_then(|a| a.first())
        .and_then(|p| p.get("text"))
        .and_then(|t| t.as_str())
        .ok_or_else(|| format!("no text in response: {}", &body[..body.len().min(200)]))?;

    let clean = text.trim();
    if clean.is_empty() {
        return Err("empty text in response".into());
    }
    // One paragraph. The page renders this as plain text in a single line box,
    // and a model that decided to produce a bulleted list would break it.
    Ok(clean
        .lines()
        .map(|l| l.trim().trim_start_matches(['-', '*', '•']).trim())
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" "))
}

/// The instruction sent with every request.
///
/// Chosen by comparing seven variants against live snapshots. Three things
/// decided it:
///
///   * It opens with where capacity is. A reader looking at this dashboard is
///     usually asking "where can I run something", not "what is the total".
///   * It forbids counting. An earlier variant asked the model to collapse a
///     list into a count and it answered "six" when there were seven; the
///     digest now counts everything itself and the model only rephrases.
///   * It bans advice. "Please take care" is not information, and this line
///     sits above a screen full of numbers that speak for themselves.
/// The instruction sent with every request.
///
/// Chosen by comparing variants against live snapshots, then rewritten when the
/// question changed. Three things it must keep doing:
///
///   * Report a verdict, not an inventory. The cards below already say who is
///     busy; a line at the top earns its place only by saying whether anything
///     is wrong.
///   * Say so when nothing is wrong. A monitor that is silent when healthy
///     cannot be distinguished from a monitor that is broken.
///   * Never count or judge. The digest decides what crosses a threshold and
///     does every sum; an earlier version that asked the model to collapse a
///     list into a count answered "six" when there were seven.
const PROMPT: &str = "\
연구실 GPU 클러스터 상태 브리핑을 씁니다. 한국어 3~4문장, 250자 이내.

아래 \"판정\"·\"이상 징후\"·\"관측\"에 적힌 사실만 사용하세요.

- **완전한 문장으로 쓰세요.** \"랙 301-513 2408 W\" 처럼 명사만 나열하지 말고,
  \"301-513 랙이 2408 W로 전력을 가장 많이 쓰고 있습니다\" 처럼 서술하세요.
- **어떤 항목을 언급할 때는 왜 언급하는지도 함께 쓰세요.** 임계값이나 이유가 적혀 있으면
  그것을 문장에 넣으세요. 예를 들어 VRAM 수치를 말한다면 그것이 왜 문제인지(추가 할당 시
  OOM 위험) 같이 말해야 읽는 사람이 심각한지 아닌지 압니다. 숫자만 던지지 마세요.
- 이상 징후가 있으면 그것부터. 없으면 \"이상 없음\"으로 시작하세요.
- 전체 규모(GPU 몇 대 중 몇 대 사용 중, 서버 응답 수, 총 전력)를 한 번 말하세요.
  화면에 따로 표시되지 않으므로 이 문장이 유일한 출처입니다.
- 그리고 전력과 열 중 의미 있는 것을 고르세요. 적혀 있는 것을 전부 나열할 필요는 없습니다.
- 직전 브리핑과 비교한 항목이 있으면, 몇 분 전과 비교한 것인지 함께 쓰세요.
- **어느 서버가 비어 있는지는 절대 쓰지 마세요.** 총계는 괜찮지만 한가한 서버의 이름을 대서는 안 됩니다.
- 세거나 계산하거나 판단하지 마세요. 임계 판정과 집계는 이미 끝나 있습니다. 숫자는 그대로 옮기세요.
- 적혀 있지 않은 것은 쓰지 마세요.
- 사실만 진술하세요. 권유하거나 지시하지 마세요.
- 마크다운, 목록, 이모지 금지. 서두 금지.";

/// VRAM this full is one allocation away from an out-of-memory failure, which
/// is worth saying before it happens rather than after.
const VRAM_ALERT: f64 = 0.92;

/// A fan this far open means the cooler has nothing left to give.
const FAN_ALERT: f64 = 95.0;

/// A GPU at or above this is hot enough to be worth a line. Consumer cards
/// throttle in the mid-eighties, so this is the point where a card is losing
/// performance rather than merely working hard.
const TEMP_ALERT: f64 = 85.0;

/// Drawing this close to the configured limit means the card is power-capped.
const POWER_ALERT: f64 = 0.95;

/// A compact, factual digest of the published snapshot.
///
/// Deliberately small: tokens cost money, and a model handed a wall of numbers
/// writes a worse summary than one handed the few that matter.
pub fn digest(snapshot: &str) -> Option<String> {
    digest_with_trend(snapshot, None)
}

/// Total draw across every responding server in a snapshot.
pub fn total_watts(snapshot: &str) -> Option<f64> {
    let v = json::parse(snapshot.trim_start_matches('\u{feff}')).ok()?;
    Some(
        v.get("servers")?
            .as_arr()?
            .iter()
            .filter(|s| s.str_or("status", "") == "ok")
            .map(|s| s.num_or("watts", 0.0))
            .sum(),
    )
}

/// As `digest`, plus what the total draw was at the previous briefing and how
/// many minutes ago that was.
pub fn digest_with_trend(snapshot: &str, trend: Option<(f64, u32)>) -> Option<String> {
    // Strip a byte-order mark. The collector never writes one, but this also
    // reads snapshots off disk, and anything edited on Windows may carry one.
    let v = json::parse(snapshot.trim_start_matches('\u{feff}')).ok()?;
    let servers = v.get("servers")?.as_arr()?;

    let mut lines = Vec::new();
    let mut down = Vec::new();
    let mut alerts = Vec::new();
    let mut hottest: Option<(String, f64)> = None;
    let mut thirstiest: Option<(String, f64, f64)> = None;
    let mut by_room: BTreeMap<String, f64> = BTreeMap::new();
    let mut vram_tight: Vec<String> = Vec::new();
    let mut fans_open: Vec<String> = Vec::new();
    let mut rebooted: Vec<String> = Vec::new();
    let (mut tot, mut busy, mut watts) = (0usize, 0usize, 0.0f64);

    for s in servers {
        let name = s.str_or("name", "?");
        if s.str_or("status", "") != "ok" {
            down.push(name.to_string());
            continue;
        }
        let gpus = match s.get("gpus").and_then(|g| g.as_arr()) {
            Some(g) => g,
            None => continue,
        };
        let n = gpus.len();
        // Same rule the page uses to colour a card.
        let b = gpus
            .iter()
            .filter(|g| {
                g.num_or("util", 0.0) >= 5.0 || g.num_or("mem_used", 0.0) > 512.0
            })
            .count();
        let w = s.num_or("watts", 0.0);
        let tmax = gpus
            .iter()
            .map(|g| g.num_or("temp", 0.0))
            .fold(0.0f64, f64::max);
        let cap: f64 = gpus.iter().map(|g| g.num_or("power_limit", 0.0)).sum();
        tot += n;
        busy += b;
        watts += w;
        *by_room.entry(s.str_or("loc", "?").to_string()).or_insert(0.0) += w;

        let vmax = gpus
            .iter()
            .filter(|g| g.num_or("mem_total", 0.0) > 0.0)
            .map(|g| g.num_or("mem_used", 0.0) / g.num_or("mem_total", 1.0))
            .fold(0.0f64, f64::max);
        if vmax >= VRAM_ALERT {
            vram_tight.push(format!("{} {}%", name, (vmax * 100.0).round() as i64));
        }
        let fmax = gpus.iter().map(|g| g.num_or("fan", 0.0)).fold(0.0f64, f64::max);
        if fmax >= FAN_ALERT {
            // Clamped for display only. nvidia-smi reports a percentage of a
            // reference maximum and some cards return more than that -- 115 is
            // a genuine reading, but printed in a sentence it reads as a bug,
            // and at that point the fan is simply at its limit.
            fans_open.push(format!("{} {}%", name, fmax.min(100.0).round() as i64));
        }
        let up = s.num_or("uptime_sec", 0.0);
        if up > 0.0 && up < 86_400.0 {
            rebooted.push(format!("{} {}시간", name, (up / 3600.0).round() as i64));
        }

        if tmax >= TEMP_ALERT {
            alerts.push(format!(
                "{}: GPU 온도 {}도 (임계 {}도)",
                name,
                tmax.round() as i64,
                TEMP_ALERT as i64
            ));
        }
        if cap > 0.0 && w / cap >= POWER_ALERT {
            alerts.push(format!(
                "{}: 전력 {} W, 제한 {} W의 {}% (전력 상한 근접)",
                name,
                w.round() as i64,
                cap.round() as i64,
                (w / cap * 100.0).round() as i64
            ));
        }
        if hottest.as_ref().map_or(true, |(_, t)| tmax > *t) {
            hottest = Some((name.to_string(), tmax));
        }
        if thirstiest.as_ref().map_or(true, |(_, pw, _)| w > *pw) {
            thirstiest = Some((name.to_string(), w, cap));
        }
        lines.push(format!(
            "{} ({}): GPU {}/{} 사용중, {} W, 최고 {}도, 랙 {}",
            name,
            s.str_or("gpu_model", ""),
            b,
            n,
            w.round() as i64,
            tmax.round() as i64,
            s.str_or("loc", "")
        ));
    }

    if lines.is_empty() && down.is_empty() {
        return None;
    }

    // The verdict is decided here, not by the model. Thresholds that drifted
    // from one call to the next would make the line untrustworthy.
    for d in &down {
        alerts.push(format!("{}: 응답 없음", d));
    }

    let mut out = if alerts.is_empty() {
        String::from("판정: 이상 없음\n")
    } else {
        format!("판정: 이상 징후 {}건\n이상 징후:\n", alerts.len())
    };
    let totals = format!(
        "전체 규모: GPU {}대 중 {}대 사용 중, 서버 {}대 중 {}대 응답",
        tot,
        busy,
        lines.len() + down.len(),
        lines.len()
    );
    for a in &alerts {
        out.push_str("- ");
        out.push_str(a);
        out.push('\n');
    }

    out.push_str("관측:\n");
    if let Some((n, pw, cap)) = thirstiest {
        out.push_str(&format!(
            "- 서버 중에서는 {}이 {} W로 전력을 가장 많이 쓰는 중{}\n",
            n,
            pw.round() as i64,
            if cap > 0.0 {
                format!(" (제한 {} W의 {}%)", cap.round() as i64, (pw / cap * 100.0).round() as i64)
            } else {
                String::new()
            }
        ));
    }
    if let Some((n, t)) = hottest {
        out.push_str(&format!(
            "- 가장 뜨거운 카드는 {}의 {}도 (경고 임계 {}도)\n",
            n,
            t.round() as i64,
            TEMP_ALERT as i64
        ));
    }
    let mut rooms: Vec<(&String, &f64)> = by_room.iter().collect();
    rooms.sort_by(|a, b| b.1.partial_cmp(a.1).unwrap_or(std::cmp::Ordering::Equal));
    if let Some((room, w)) = rooms.first() {
        out.push_str(&format!(
            "- 랙 중에서는 {}이 {} W로 전력을 가장 많이 쓰는 중\n",
            room,
            w.round() as i64
        ));
    }
    // Each label carries the reason the figure is worth a mention. A bare
    // "VRAM 95%" leaves the reader unable to tell alarming from ordinary, and
    // gives no hint why these machines and not the other eleven.
    if !vram_tight.is_empty() {
        out.push_str(&format!(
            "- VRAM이 {}% 이상 차서 추가 할당 시 OOM 위험: {}\n",
            (VRAM_ALERT * 100.0).round() as i64,
            vram_tight.join(", ")
        ));
    }
    if !fans_open.is_empty() {
        out.push_str(&format!(
            "- 팬이 {}% 이상으로 냉각 여력이 없음: {}\n",
            FAN_ALERT as i64,
            fans_open.join(", ")
        ));
    }
    if !rebooted.is_empty() {
        out.push_str(&format!(
            "- 24시간 안에 재부팅되어 실행 중이던 작업이 끊겼을 수 있음: {}\n",
            rebooted.join(", ")
        ));
    }
    if let Some((prev, mins)) = trend {
        let d = watts - prev;
        out.push_str(&format!(
            "- 직전 브리핑({}분 전)과 비교하면 전력이 {} W에서 {} W로 {}{} W 변함\n",
            mins,
            prev.round() as i64,
            watts.round() as i64,
            if d >= 0.0 { "+" } else { "" },
            d.round() as i64
        ));
    }

    // These used to be on screen in their own boxes and were marked "do not
    // repeat". The boxes are gone, so the briefing is the only place they
    // appear now.
    out.push_str(&format!("- {}, 총 {} W\n", totals, watts.round() as i64));
    out.push_str("\n서버별:\n");
    out.push_str(&lines.join("\n"));
    Some(out)
}

/// The published file. Small and separate so the page can fetch it on its own
/// cadence without touching the once-a-second snapshot.
pub fn write_json(text: &str, ts: u32, model: &str) -> String {
    let mut w = json::Writer::new();
    w.raw("{");
    w.key("ts");
    w.num(ts as f64);
    w.raw(",");
    w.key("model");
    w.str(model);
    w.raw(",");
    w.key("text");
    w.str(text);
    w.raw("}");
    w.buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn curl_config_escapes_windows_paths() {
        let c = curl_config("m", "k", r"C:\Users\me\AppData\Local\Temp\body.json", 30);
        assert!(
            c.contains("data-binary = \"@C:/Users/me/AppData/Local/Temp/body.json\""),
            "a backslash in a curl config is an escape; the path must use forward slashes:\n{}",
            c
        );
        assert!(!c.contains("\\"), "no backslash may survive into the config:\n{}", c);
    }

    #[test]
    fn extract_reads_the_answer() {
        let body = r#"{"candidates":[{"content":{"parts":[{"text":"  hi22가 가득 찼습니다.  "}]}}]}"#;
        assert_eq!(extract(body).unwrap(), "hi22가 가득 찼습니다.");
    }

    #[test]
    fn extract_refuses_a_truncated_answer() {
        // What a spent thinking budget actually returns.
        let body = r#"{"candidates":[{"finishReason":"MAX_TOKENS",
                       "content":{"parts":[{"text":"전체 GPU 83개 중"}]}}]}"#;
        let e = extract(body).unwrap_err();
        assert!(e.contains("MAX_TOKENS"), "{}", e);
    }

    #[test]
    fn extract_accepts_an_explicit_clean_finish() {
        let body = r#"{"candidates":[{"finishReason":"STOP",
                       "content":{"parts":[{"text":"조용합니다."}]}}]}"#;
        assert_eq!(extract(body).unwrap(), "조용합니다.");
    }

    #[test]
    fn extract_flattens_a_list_the_model_should_not_have_sent() {
        let body = r#"{"candidates":[{"content":{"parts":[{"text":"- hi22 가득\n- hi27 유휴"}]}}]}"#;
        assert_eq!(extract(body).unwrap(), "hi22 가득 hi27 유휴");
    }

    #[test]
    fn extract_surfaces_the_api_error_instead_of_a_parse_failure() {
        let body = r#"{"error":{"code":429,"message":"Quota exceeded"}}"#;
        let e = extract(body).unwrap_err();
        assert!(e.contains("429"), "{}", e);
        assert!(e.contains("Quota exceeded"), "{}", e);
    }

    #[test]
    fn extract_rejects_a_response_with_no_text() {
        assert!(extract(r#"{"candidates":[]}"#).is_err());
        assert!(extract("not json at all").is_err());
    }

    #[test]
    fn digest_reads_the_published_snapshot() {
        let snap = r#"{"servers":[
            {"name":"g1","status":"ok","gpu_model":"3090","loc":"a","watts":900,
             "gpus":[{"util":97,"mem_used":20000,"temp":70},
                     {"util":0,"mem_used":10,"temp":35}]},
            {"name":"g2","status":"down","gpu_model":"4090","loc":"b"}
        ]}"#;
        let d = digest(snap).unwrap();
        assert!(d.contains("GPU 1/2 사용중"), "{}", d);
        assert!(d.contains("g2: 응답 없음"), "{}", d);
        assert!(d.contains("g1 (3090): GPU 1/2 사용중, 900 W, 최고 70도, 랙 a"), "{}", d);
        assert!(d.contains("g1이 900 W로 전력을 가장 많이 쓰는 중"), "{}", d);
        assert!(d.contains("가장 뜨거운 카드는 g1의 70도 (경고 임계 85도)"), "{}", d);
    }

    #[test]
    fn digest_counts_idle_servers_itself() {
        // The model must never be the thing that counts: asked to, it said six
        // when there were seven.
        let mut servers = Vec::new();
        for i in 1..=7 {
            servers.push(format!(
                r#"{{"name":"idle{}","status":"ok","gpu_model":"x","loc":"r","watts":90,
                    "gpus":[{{"util":0,"mem_used":5,"temp":30}}]}}"#,
                i
            ));
        }
        servers.push(
            r#"{"name":"busy1","status":"ok","gpu_model":"x","loc":"r","watts":900,
               "gpus":[{"util":99,"mem_used":9000,"temp":88}]}"#
                .to_string(),
        );
        let snap = format!(r#"{{"servers":[{}]}}"#, servers.join(","));
        let d = digest(&snap).unwrap();
        assert!(d.contains("판정: 이상 징후"), "{}", d);
        assert!(d.contains("busy1: GPU 온도 88도"), "{}", d);
        assert!(d.contains("가장 뜨거운 카드는 busy1의 88도"), "{}", d);
    }

    #[test]
    fn a_healthy_cluster_is_reported_as_healthy() {
        // Silence when healthy is indistinguishable from a broken monitor.
        let snap = r#"{"servers":[
            {"name":"g1","status":"ok","gpu_model":"x","loc":"r","watts":300,
             "gpus":[{"util":80,"mem_used":9000,"temp":62,"power_limit":350}]}
        ]}"#;
        let d = digest(snap).unwrap();
        assert!(d.starts_with("판정: 이상 없음"), "{}", d);
        assert!(!d.contains("이상 징후:"), "{}", d);
    }

    #[test]
    fn a_card_at_its_power_limit_is_an_anomaly() {
        let snap = r#"{"servers":[
            {"name":"g1","status":"ok","gpu_model":"x","loc":"r","watts":345,
             "gpus":[{"util":99,"mem_used":9000,"temp":70,"power_limit":350}]}
        ]}"#;
        let d = digest(snap).unwrap();
        assert!(d.contains("전력 상한 근접"), "{}", d);
        assert!(d.contains("99%"), "{}", d); // 345/350 = 98.6, rounded
    }

    #[test]
    fn the_briefing_never_reports_spare_capacity() {
        // Naming the empty machines turns this line into a queue ticket and
        // people race for them. It is a product decision, so it is pinned here
        // rather than left to the wording of a prompt.
        let snap = r#"{"servers":[
            {"name":"idle1","status":"ok","gpu_model":"x","loc":"roomA","watts":90,
             "gpus":[{"util":0,"mem_used":4,"mem_total":24000,"temp":30,"power_limit":350,"fan":30}]},
            {"name":"busy1","status":"ok","gpu_model":"x","loc":"roomB","watts":900,
             "gpus":[{"util":99,"mem_used":23000,"mem_total":24000,"temp":70,"power_limit":1000,"fan":99}]}
        ]}"#;
        let d = digest(snap).unwrap();
        // Aggregates are fine -- "one of two in use" names nobody. What must
        // never appear is a conclusion that points at the free machine, which
        // is what turns the line into a queue ticket.
        for banned in ["비어 있는 서버", "여유 서버", "유휴 서버", "사용 가능한 서버"] {
            assert!(!d.contains(banned), "digest leaked spare capacity ({}):\n{}", banned, d);
        }
        assert!(d.contains("전체 규모: GPU 2대 중 1대 사용 중"), "{}", d);
    }

    #[test]
    fn the_briefing_carries_power_heat_and_memory() {
        let snap = r#"{"servers":[
            {"name":"g1","status":"ok","gpu_model":"x","loc":"roomA","watts":120,
             "gpus":[{"util":10,"mem_used":100,"mem_total":24000,"temp":40,"power_limit":350,"fan":30}]},
            {"name":"g2","status":"ok","gpu_model":"x","loc":"roomB","watts":900,
             "gpus":[{"util":99,"mem_used":23500,"mem_total":24000,"temp":70,"power_limit":1000,"fan":98}]}
        ]}"#;
        let d = digest_with_trend(snap, Some((800.0, 10))).unwrap();
        assert!(d.contains("roomB이 900 W로 전력을 가장 많이 쓰는 중"), "{}", d);
        // The reason travels with the number, so the model cannot drop it.
        assert!(d.contains("VRAM이 92% 이상 차서 추가 할당 시 OOM 위험: g2 98%"), "{}", d);
        assert!(d.contains("팬이 95% 이상으로 냉각 여력이 없음: g2 98%"), "{}", d);
        // 1020 now vs 800 an hour ago.
        assert!(
            d.contains("직전 브리핑(10분 전)과 비교하면 전력이 800 W에서 1020 W로 +220 W 변함"),
            "{}",
            d
        );
        assert!(d.contains("서버 2대 중 2대 응답"), "{}", d);
    }

    #[test]
    fn a_fan_above_its_reference_maximum_reads_as_100() {
        let snap = r#"{"servers":[
            {"name":"g1","status":"ok","gpu_model":"x","loc":"r","watts":300,
             "gpus":[{"util":90,"mem_used":100,"mem_total":24000,"temp":70,
                      "power_limit":350,"fan":115}]}
        ]}"#;
        let d = digest(snap).unwrap();
        assert!(d.contains("냉각 여력이 없음: g1 100%"), "{}", d);
        assert!(!d.contains("115"), "a percentage over 100 reads as a bug:\n{}", d);
    }

    #[test]
    fn total_watts_counts_only_responding_servers() {
        let snap = r#"{"servers":[
            {"name":"a","status":"ok","watts":100,"gpus":[]},
            {"name":"b","status":"ok","watts":250,"gpus":[]},
            {"name":"c","status":"down","watts":9999}
        ]}"#;
        assert_eq!(total_watts(snap), Some(350.0));
    }

    #[test]
    fn digest_tolerates_a_byte_order_mark() {
        let snap = "\u{feff}{\"servers\":[{\"name\":\"g1\",\"status\":\"ok\",\"gpu_model\":\"x\",\
                    \"loc\":\"r\",\"watts\":90,\"gpus\":[{\"util\":0,\"mem_used\":4,\"temp\":30}]}]}";
        assert!(digest(snap).is_some(), "a BOM must not make the snapshot unreadable");
    }

    #[test]
    fn digest_of_nothing_is_nothing() {
        assert!(digest(r#"{"servers":[]}"#).is_none());
        assert!(digest("garbage").is_none());
    }

    #[test]
    fn a_missing_key_file_disables_the_feature_quietly() {
        let s = Summarizer::new(
            std::env::temp_dir().join("hilmon-no-such-key-file"),
            "m".into(),
        );
        assert!(!s.enabled());
        assert!(s.key().is_none());
    }
}
