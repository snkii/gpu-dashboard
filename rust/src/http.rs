// Copyright (c) 2026 Seonuk Kim, Human Interface Laboratory, Seoul National University
// SPDX-License-Identifier: MIT
//! The dashboard server.
//!
//! Viewer load and SSH load are completely decoupled: a request only ever
//! reads the last snapshot out of memory. A dozen people refreshing once a
//! second cause ZERO extra SSH connections -- nothing in this file can open
//! one.

use crate::collect::Collector;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

/// The page is small and static; only status.json changes. Serialising it once
/// per snapshot keeps the work O(1) in the number of viewers.
struct Cache {
    body: Vec<u8>,
    etag: String,
    stamp: u64,
}

pub struct Server {
    collector: Arc<Collector>,
    web: PathBuf,
    public: bool,
    cache: Mutex<Cache>,
    // (built_at, body) -- the statistics file, rebuilt at most once a minute.
    stats: Mutex<(u32, Vec<u8>)>,
    stop: Arc<AtomicBool>,
}

const SECURITY_HEADERS: &[(&str, &str)] = &[
    // The page loads nothing from anywhere else, so the policy can be this
    // tight. 'unsafe-inline' covers the single inline <script>/<style>.
    ("Content-Security-Policy",
     "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; \
      connect-src 'self'; img-src 'self' data:; manifest-src 'self'; base-uri 'none'; \
      form-action 'none'; frame-ancestors 'none'"),
    ("X-Content-Type-Options", "nosniff"),
    ("X-Frame-Options", "DENY"),
    ("Referrer-Policy", "no-referrer"),
    ("Permissions-Policy", "geolocation=(), camera=(), microphone=()"),
    ("Cross-Origin-Opener-Policy", "same-origin"),
    ("Cross-Origin-Resource-Policy", "same-origin"),
    ("Strict-Transport-Security", "max-age=63072000; includeSubDomains"),
];

fn content_type(path: &str) -> Option<&'static str> {
    // Whitelist, not a lookup with a fallback: an unlisted extension is simply
    // not served, so no source file can be fetched by guessing a name.
    Some(match Path::new(path).extension()?.to_str()? {
        "html" => "text/html; charset=utf-8",
        "json" => "application/json; charset=utf-8",
        "webmanifest" => "application/manifest+json",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "svg" => "image/svg+xml",
        "webp" => "image/webp",
        "css" => "text/css",
        "js" => "text/javascript",
        _ => return None,
    })
}

impl Server {
    pub fn new(collector: Arc<Collector>, web: PathBuf, public: bool) -> Arc<Server> {
        Arc::new(Server {
            collector,
            web,
            public,
            cache: Mutex::new(Cache {
                body: Vec::new(),
                etag: String::new(),
                stamp: u64::MAX,
            }),
            stats: Mutex::new((0, Vec::new())),
            stop: Arc::new(AtomicBool::new(false)),
        })
    }

    pub fn serve(self: &Arc<Self>, addr: &str) -> std::io::Result<()> {
        let listener = TcpListener::bind(addr)?;
        for stream in listener.incoming() {
            if self.stop.load(Ordering::SeqCst) {
                break;
            }
            let s = Arc::clone(self);
            match stream {
                Ok(st) => {
                    thread::spawn(move || {
                        let _ = s.handle(st);
                    });
                }
                Err(_) => continue,
            }
        }
        Ok(())
    }

    fn status_body(&self) -> (Vec<u8>, String) {
        let samples = self.collector.samples();
        // Newest sample timestamp identifies the snapshot; if nothing moved,
        // neither did the bytes.
        let stamp = samples.values().map(|s| s.ts).max().unwrap_or(0);
        {
            let c = self.cache.lock().unwrap();
            if c.stamp == stamp && !c.body.is_empty() {
                return (c.body.clone(), c.etag.clone());
            }
        }
        let body = self.collector.snapshot_json(self.public).into_bytes();
        let etag = format!("\"{:x}\"", fnv1a(&body));
        let mut c = self.cache.lock().unwrap();
        c.body = body.clone();
        c.etag = etag.clone();
        c.stamp = stamp;
        (body, etag)
    }

    fn handle(&self, mut st: TcpStream) -> std::io::Result<()> {
        st.set_read_timeout(Some(std::time::Duration::from_secs(15)))?;
        let mut reader = BufReader::new(st.try_clone()?);
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Ok(());
        }
        let mut parts = line.split_whitespace();
        let method = parts.next().unwrap_or("");
        let target = parts.next().unwrap_or("/");

        let mut headers = HashMap::new();
        loop {
            let mut h = String::new();
            if reader.read_line(&mut h)? == 0 || h.trim().is_empty() {
                break;
            }
            if let Some((k, v)) = h.split_once(':') {
                headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
            }
        }

        if method != "GET" && method != "HEAD" {
            return self.send(&mut st, 405, b"method not allowed",
                             "text/plain; charset=utf-8", None, method == "HEAD");
        }

        let path = target.split('?').next().unwrap_or("/");
        let head = method == "HEAD";

        if path == "/" || path == "/index.html" {
            let p = self.web.join("index.html");
            return match std::fs::read(&p) {
                Ok(b) => {
                    let etag = format!("\"{:x}\"", fnv1a(&b));
                    self.send(&mut st, 200, &b, "text/html; charset=utf-8",
                              Some((&etag, headers.get("if-none-match"))), head)
                }
                Err(_) => self.send(&mut st, 500, b"web/index.html missing",
                                    "text/plain; charset=utf-8", None, head),
            };
        }

        if path == "/stats.json" || path == "/api/stats" {
            // Rebuilt at most once a minute: it reads days of history off disk
            // and a dozen viewers must not each trigger that.
            let now = crate::gate::now() as u32;
            let body = {
                let mut c = self.stats.lock().unwrap();
                if c.1.is_empty() || now.saturating_sub(c.0) >= 60 {
                    let ranges = self.collector.stats_ranges(now);
                    *c = (now, self.collector.stats_json(&ranges, now).into_bytes());
                }
                c.1.clone()
            };
            let etag = format!("\"{:x}\"", fnv1a(&body));
            return self.send(&mut st, 200, &body, "application/json; charset=utf-8",
                             Some((&etag, headers.get("if-none-match"))), head);
        }

        if path == "/status.json" || path == "/api/status" {
            let (body, etag) = self.status_body();
            return self.send(&mut st, 200, &body, "application/json; charset=utf-8",
                             Some((&etag, headers.get("if-none-match"))), head);
        }

        self.serve_static(&mut st, path, headers.get("if-none-match"), head)
    }

    fn serve_static(
        &self,
        st: &mut TcpStream,
        path: &str,
        inm: Option<&String>,
        head: bool,
    ) -> std::io::Result<()> {
        let rel = percent_decode(path.trim_start_matches('/'));
        let ct = match content_type(&rel) {
            Some(c) => c,
            None => {
                return self.send(st, 404, b"not found", "text/plain; charset=utf-8", None, head)
            }
        };

        // Reject traversal on the COMPONENTS, before touching the filesystem.
        // Checking the resolved path alone is not enough on Windows, where a
        // path need not exist to be canonicalised.
        let candidate = Path::new(&rel);
        if candidate.components().any(|c| !matches!(c, Component::Normal(_))) {
            return self.send(st, 404, b"not found", "text/plain; charset=utf-8", None, head);
        }
        let full = self.web.join(candidate);
        let (root, target) = match (self.web.canonicalize(), full.canonicalize()) {
            (Ok(r), Ok(t)) => (r, t),
            _ => return self.send(st, 404, b"not found", "text/plain; charset=utf-8", None, head),
        };
        if !target.starts_with(&root) || !target.is_file() {
            return self.send(st, 404, b"not found", "text/plain; charset=utf-8", None, head);
        }

        let mut body = Vec::new();
        std::fs::File::open(&target)?.read_to_end(&mut body)?;
        let etag = format!("\"{:x}\"", fnv1a(&body));
        self.send(st, 200, &body, ct, Some((&etag, inm)), head)
    }

    fn send(
        &self,
        st: &mut TcpStream,
        code: u16,
        body: &[u8],
        ct: &str,
        etag: Option<(&str, Option<&String>)>,
        head: bool,
    ) -> std::io::Result<()> {
        // A 1 Hz dashboard mostly asks for data that has not changed; answer
        // those with 304 and no body at all.
        if let Some((tag, Some(inm))) = etag {
            if inm == tag {
                let mut h = format!(
                    "HTTP/1.1 304 Not Modified\r\nETag: {}\r\nCache-Control: no-cache\r\n",
                    tag
                );
                for (k, v) in SECURITY_HEADERS {
                    h.push_str(&format!("{}: {}\r\n", k, v));
                }
                h.push_str("\r\n");
                return st.write_all(h.as_bytes());
            }
        }

        let reason = match code {
            200 => "OK",
            404 => "Not Found",
            405 => "Method Not Allowed",
            _ => "Internal Server Error",
        };
        let mut h = format!("HTTP/1.1 {} {}\r\n", code, reason);
        h.push_str(&format!("Content-Type: {}\r\n", ct));
        h.push_str(&format!("Content-Length: {}\r\n", body.len()));
        match etag {
            Some((tag, _)) => {
                h.push_str(&format!("ETag: {}\r\nCache-Control: no-cache\r\n", tag))
            }
            None => h.push_str("Cache-Control: no-store\r\n"),
        }
        for (k, v) in SECURITY_HEADERS {
            h.push_str(&format!("{}: {}\r\n", k, v));
        }
        h.push_str("Connection: close\r\n\r\n");
        st.write_all(h.as_bytes())?;
        if !head {
            st.write_all(body)?;
        }
        st.flush()
    }
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in data {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_whitelisted_extensions_are_servable() {
        assert!(content_type("index.html").is_some());
        assert!(content_type("icons/favicon.ico").is_some());
        assert!(content_type("hilmon.py").is_none());
        assert!(content_type("servers.json").is_some(), "json is allowed by type");
        assert!(content_type("Cargo.toml").is_none());
        assert!(content_type("noext").is_none());
    }

    #[test]
    fn percent_decoding_handles_escaped_traversal() {
        assert_eq!(percent_decode("icons%2Ffavicon.ico"), "icons/favicon.ico");
        assert_eq!(percent_decode("%2e%2e/x"), "../x");
        assert_eq!(percent_decode("plain"), "plain");
    }

    #[test]
    fn traversal_components_are_rejected() {
        // The decoded form is what the component check sees.
        for bad in ["../servers.json", "icons/../../hilmon.py", "%2e%2e/x"] {
            let rel = percent_decode(bad);
            let has_escape = Path::new(&rel)
                .components()
                .any(|c| !matches!(c, Component::Normal(_)));
            assert!(has_escape, "{:?} should be rejected", bad);
        }
        let ok = percent_decode("icons/icon-192.png");
        assert!(Path::new(&ok)
            .components()
            .all(|c| matches!(c, Component::Normal(_))));
    }

    #[test]
    fn etag_changes_with_content() {
        assert_ne!(fnv1a(b"a"), fnv1a(b"b"));
        assert_eq!(fnv1a(b"same"), fnv1a(b"same"));
    }
}
