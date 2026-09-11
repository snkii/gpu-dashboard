// Copyright (c) 2026 Seonuk Kim, Human Interface Laboratory, Seoul National University
// SPDX-License-Identifier: MIT
//! A minimal JSON reader/writer.
//!
//! Only what this program needs: parse `servers.json`, emit `status.json`.
//! Writing a few hundred lines here keeps the build dependency-free, which
//! matters more than generality for a tool that has to compile on a lab
//! machine with no network and ship as one file.

use std::collections::BTreeMap;
use std::fmt::Write as _;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Value>),
    // BTreeMap, not HashMap: a stable key order makes the emitted JSON
    // byte-identical between runs, so an unchanged snapshot really is
    // unchanged and the ETag/upload skip actually fires.
    Obj(BTreeMap<String, Value>),
}

impl Value {
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Obj(m) => m.get(key),
            _ => None,
        }
    }
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Num(n) => Some(*n),
            _ => None,
        }
    }
    pub fn as_u64(&self) -> Option<u64> {
        self.as_f64().map(|n| n as u64)
    }
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }
    pub fn as_arr(&self) -> Option<&Vec<Value>> {
        match self {
            Value::Arr(a) => Some(a),
            _ => None,
        }
    }
    pub fn str_or<'a>(&'a self, key: &str, dflt: &'a str) -> &'a str {
        self.get(key).and_then(|v| v.as_str()).unwrap_or(dflt)
    }
    pub fn num_or(&self, key: &str, dflt: f64) -> f64 {
        self.get(key).and_then(|v| v.as_f64()).unwrap_or(dflt)
    }
    pub fn bool_or(&self, key: &str, dflt: bool) -> bool {
        self.get(key).and_then(|v| v.as_bool()).unwrap_or(dflt)
    }
}

// ---------------------------------------------------------------- parsing

pub fn parse(src: &str) -> Result<Value, String> {
    let b: Vec<char> = src.chars().collect();
    let mut p = Parser { b, i: 0 };
    p.ws();
    let v = p.value()?;
    p.ws();
    if p.i < p.b.len() {
        return Err(format!("trailing input at char {}", p.i));
    }
    Ok(v)
}

struct Parser {
    b: Vec<char>,
    i: usize,
}

impl Parser {
    fn peek(&self) -> Option<char> {
        self.b.get(self.i).copied()
    }
    fn ws(&mut self) {
        while matches!(self.peek(), Some(c) if c.is_whitespace()) {
            self.i += 1;
        }
    }
    fn eat(&mut self, c: char) -> Result<(), String> {
        if self.peek() == Some(c) {
            self.i += 1;
            Ok(())
        } else {
            Err(format!("expected {:?} at char {}", c, self.i))
        }
    }

    fn value(&mut self) -> Result<Value, String> {
        match self.peek() {
            Some('{') => self.object(),
            Some('[') => self.array(),
            Some('"') => Ok(Value::Str(self.string()?)),
            Some('t') => self.lit("true", Value::Bool(true)),
            Some('f') => self.lit("false", Value::Bool(false)),
            Some('n') => self.lit("null", Value::Null),
            Some(c) if c == '-' || c.is_ascii_digit() => self.number(),
            other => Err(format!("unexpected {:?} at char {}", other, self.i)),
        }
    }

    fn lit(&mut self, word: &str, v: Value) -> Result<Value, String> {
        for c in word.chars() {
            self.eat(c)?;
        }
        Ok(v)
    }

    fn object(&mut self) -> Result<Value, String> {
        self.eat('{')?;
        let mut m = BTreeMap::new();
        self.ws();
        if self.peek() == Some('}') {
            self.i += 1;
            return Ok(Value::Obj(m));
        }
        loop {
            self.ws();
            let k = self.string()?;
            self.ws();
            self.eat(':')?;
            self.ws();
            let v = self.value()?;
            m.insert(k, v);
            self.ws();
            match self.peek() {
                Some(',') => self.i += 1,
                Some('}') => {
                    self.i += 1;
                    return Ok(Value::Obj(m));
                }
                other => return Err(format!("expected , or }} got {:?}", other)),
            }
        }
    }

    fn array(&mut self) -> Result<Value, String> {
        self.eat('[')?;
        let mut a = Vec::new();
        self.ws();
        if self.peek() == Some(']') {
            self.i += 1;
            return Ok(Value::Arr(a));
        }
        loop {
            self.ws();
            a.push(self.value()?);
            self.ws();
            match self.peek() {
                Some(',') => self.i += 1,
                Some(']') => {
                    self.i += 1;
                    return Ok(Value::Arr(a));
                }
                other => return Err(format!("expected , or ] got {:?}", other)),
            }
        }
    }

    fn string(&mut self) -> Result<String, String> {
        self.eat('"')?;
        let mut s = String::new();
        loop {
            let c = self.peek().ok_or("unterminated string")?;
            self.i += 1;
            match c {
                '"' => return Ok(s),
                '\\' => {
                    let e = self.peek().ok_or("unterminated escape")?;
                    self.i += 1;
                    match e {
                        '"' => s.push('"'),
                        '\\' => s.push('\\'),
                        '/' => s.push('/'),
                        'b' => s.push('\u{8}'),
                        'f' => s.push('\u{c}'),
                        'n' => s.push('\n'),
                        'r' => s.push('\r'),
                        't' => s.push('\t'),
                        'u' => {
                            let hex: String = (0..4)
                                .filter_map(|_| {
                                    let c = self.peek();
                                    self.i += 1;
                                    c
                                })
                                .collect();
                            let n = u32::from_str_radix(&hex, 16)
                                .map_err(|_| format!("bad \\u escape {:?}", hex))?;
                            // Surrogate pairs: the config carries Korean text,
                            // which some editors write as escaped UTF-16.
                            if (0xD800..0xDC00).contains(&n) {
                                self.eat('\\')?;
                                self.eat('u')?;
                                let hex2: String = (0..4)
                                    .filter_map(|_| {
                                        let c = self.peek();
                                        self.i += 1;
                                        c
                                    })
                                    .collect();
                                let lo = u32::from_str_radix(&hex2, 16)
                                    .map_err(|_| "bad low surrogate".to_string())?;
                                let cp = 0x10000 + ((n - 0xD800) << 10) + (lo - 0xDC00);
                                s.push(char::from_u32(cp).ok_or("bad surrogate pair")?);
                            } else {
                                s.push(char::from_u32(n).ok_or("bad code point")?);
                            }
                        }
                        _ => return Err(format!("bad escape \\{}", e)),
                    }
                }
                _ => s.push(c),
            }
        }
    }

    fn number(&mut self) -> Result<Value, String> {
        let start = self.i;
        if self.peek() == Some('-') {
            self.i += 1;
        }
        while matches!(self.peek(), Some(c) if c.is_ascii_digit() || c == '.' || c == 'e' || c == 'E' || c == '+' || c == '-')
        {
            self.i += 1;
        }
        let s: String = self.b[start..self.i].iter().collect();
        s.parse::<f64>()
            .map(Value::Num)
            .map_err(|_| format!("bad number {:?}", s))
    }
}

// ---------------------------------------------------------------- writing

/// Builder for compact JSON output.
pub struct Writer {
    pub buf: String,
}

impl Writer {
    pub fn new() -> Self {
        Writer {
            buf: String::with_capacity(32 * 1024),
        }
    }

    pub fn raw(&mut self, s: &str) {
        self.buf.push_str(s);
    }

    pub fn key(&mut self, k: &str) {
        self.str(k);
        self.buf.push(':');
    }

    pub fn str(&mut self, s: &str) {
        self.buf.push('"');
        for c in s.chars() {
            match c {
                '"' => self.buf.push_str("\\\""),
                '\\' => self.buf.push_str("\\\\"),
                '\n' => self.buf.push_str("\\n"),
                '\r' => self.buf.push_str("\\r"),
                '\t' => self.buf.push_str("\\t"),
                // Control characters must be escaped; everything else, Korean
                // included, is emitted as UTF-8 rather than \u escapes.
                c if (c as u32) < 0x20 => {
                    let _ = write!(self.buf, "\\u{:04x}", c as u32);
                }
                c => self.buf.push(c),
            }
        }
        self.buf.push('"');
    }

    /// Numbers are written without a trailing ".0" so the output matches what
    /// the Python version produced; the page compares these as numbers anyway,
    /// but identical bytes keep the ETag stable across a rewrite.
    pub fn num(&mut self, n: f64) {
        if !n.is_finite() {
            self.buf.push_str("null");
        } else if n.fract() == 0.0 && n.abs() < 1e15 {
            let _ = write!(self.buf, "{}", n as i64);
        } else {
            let _ = write!(self.buf, "{}", (n * 1000.0).round() / 1000.0);
        }
    }

    pub fn opt_num(&mut self, n: Option<f64>) {
        match n {
            Some(v) => self.num(v),
            None => self.buf.push_str("null"),
        }
    }

    pub fn bool(&mut self, b: bool) {
        self.buf.push_str(if b { "true" } else { "false" });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nested_and_unicode() {
        let v = parse(r#"{"a":[1,2.5,-3],"b":{"c":"Room A 207"},"d":true,"e":null}"#)
            .unwrap();
        assert_eq!(v.get("a").unwrap().as_arr().unwrap().len(), 3);
        assert_eq!(v.get("a").unwrap().as_arr().unwrap()[1].as_f64(), Some(2.5));
        assert_eq!(
            v.get("b").unwrap().get("c").unwrap().as_str(),
            Some("Room A 207")
        );
        assert_eq!(v.get("d").unwrap().as_bool(), Some(true));
        assert_eq!(v.get("e"), Some(&Value::Null));
    }

    #[test]
    fn rejects_trailing_junk() {
        assert!(parse(r#"{"a":1} x"#).is_err());
    }

    #[test]
    fn writes_escapes_and_numbers() {
        let mut w = Writer::new();
        w.raw("{");
        w.key("s");
        w.str("a\"b\\c\nd");
        w.raw(",");
        w.key("i");
        w.num(42.0);
        w.raw(",");
        w.key("f");
        w.num(1.25);
        w.raw(",");
        w.key("n");
        w.opt_num(None);
        w.raw("}");
        assert_eq!(
            w.buf,
            r#"{"s":"a\"b\\c\nd","i":42,"f":1.25,"n":null}"#
        );
    }

    #[test]
    fn whole_numbers_lose_the_decimal_point() {
        // The page reads these as numbers either way, but identical bytes
        // between runs are what let an unchanged snapshot skip the upload.
        let mut w = Writer::new();
        w.num(250.0);
        w.raw(" ");
        w.num(249.46);
        assert_eq!(w.buf, "250 249.46");
    }

    #[test]
    fn round_trips_korean() {
        let src = r#"{"room":"132동 B09 무향실"}"#;
        let v = parse(src).unwrap();
        let mut w = Writer::new();
        w.raw("{");
        w.key("room");
        w.str(v.get("room").unwrap().as_str().unwrap());
        w.raw("}");
        assert_eq!(w.buf, src);
    }
}
