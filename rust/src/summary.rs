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
        let facts = match digest(snapshot) {
            Some(f) => f,
            None => return false,
        };
        match self.ask_with(&key, &self.prompt, &facts) {
            Ok(t) => {
                self.failures = 0;
                let changed = t != self.text;
                self.text = t;
                self.ts = now;
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

        let out = Command::new("curl").arg("--config").arg(&cfg_path).output();
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
연구실 GPU 클러스터의 이상 감지 결과를 한 줄로 보고합니다. 터미널 출력처럼 간결하게.

아래 \"판정\"과 \"이상 징후\", \"관측\"에 적힌 사실만 사용해서 한국어로 쓰세요. 100자 이내.

- 판정이 \"이상 없음\"이면: 이상이 없다고 먼저 말하고, 관측에서 전력 최다와 최고 온도를 한 문장으로 덧붙이세요.
- 이상 징후가 있으면: 그것부터 쓰세요. 여러 건이면 심각한 것 위주로 최대 두 건까지.
- 세거나 계산하거나 판단하지 마세요. 임계 판정과 집계는 이미 끝나 있습니다. 숫자는 그대로 옮기세요.
- 적혀 있지 않은 것은 쓰지 마세요. \"참고 수치\"는 화면에 이미 있으니 반복하지 마세요.
- 사실만 진술하세요. 권유하거나 지시하지 마세요(\"주의하십시오\" 같은 말 금지).
- 마크다운, 목록, 이모지 금지. \"요약하자면\" 같은 서두도 금지.";

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
    // Strip a byte-order mark. The collector never writes one, but this also
    // reads snapshots off disk, and anything edited on Windows may carry one.
    let v = json::parse(snapshot.trim_start_matches('\u{feff}')).ok()?;
    let servers = v.get("servers")?.as_arr()?;

    let mut lines = Vec::new();
    let mut down = Vec::new();
    let mut alerts = Vec::new();
    let mut hottest: Option<(String, f64)> = None;
    let mut thirstiest: Option<(String, f64, f64)> = None;
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
            "{} ({}): GPU {}/{} 사용중, {} W, 최고 {}도, 위치 {}",
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
    for a in &alerts {
        out.push_str("- ");
        out.push_str(a);
        out.push('\n');
    }

    out.push_str("관측:\n");
    if let Some((n, pw, cap)) = thirstiest {
        out.push_str(&format!(
            "- 전력 최다: {} {} W{}\n",
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
        out.push_str(&format!("- 최고 온도: {} {}도\n", n, t.round() as i64));
    }

    out.push_str(&format!(
        "\n참고 수치 (요약에 반복하지 마세요): 전체 GPU {}/{} 사용중, 총 {} W\n\n서버별:\n",
        busy,
        tot,
        watts.round() as i64
    ));
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
        assert!(d.contains("g1 (3090): GPU 1/2 사용중, 900 W, 최고 70도, 위치 a"), "{}", d);
        assert!(d.contains("전력 최다: g1 900 W"), "{}", d);
        assert!(d.contains("최고 온도: g1 70도"), "{}", d);
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
        assert!(d.contains("최고 온도: busy1 88도"), "{}", d);
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
