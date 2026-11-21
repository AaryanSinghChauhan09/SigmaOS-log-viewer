// SPDX-License-Identifier: Apache-2.0
//!
//! SigmaOS-log-viewer — structured log viewer & query engine.
//!
//! Parses common log formats (syslog, journald-style, plain timestamped),
//! indexes entries by time/level/unit, supports filtering (level, unit,
//! grep-pattern, time range), pagination, and statistics.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

/// Parsed log entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogEntry {
    /// unix timestamp seconds (0 if unparseable).
    pub ts: u64,
    /// syslog-style priority level name.
    pub level: LogLevel,
    /// emitting unit/process (syslog tag or journald unit).
    pub unit: String,
    /// raw message body.
    pub message: String,
    /// raw line for display.
    pub raw: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LogLevel {
    Emergency,
    Alert,
    Critical,
    Error,
    Warning,
    Notice,
    Info,
    Debug,
    Unknown,
}
impl LogLevel {
    pub fn from_syslog_pri(pri: u8) -> Self {
        match pri % 8 {
            0 => Self::Emergency, 1 => Self::Alert, 2 => Self::Critical,
            3 => Self::Error, 4 => Self::Warning, 5 => Self::Notice,
            6 => Self::Info, 7 => Self::Debug, _ => Self::Unknown,
        }
    }
    pub fn name(&self) -> &'static str {
        match self {
            Self::Emergency=>"EMERG", Self::Alert=>"ALERT", Self::Critical=>"CRIT",
            Self::Error=>"ERR", Self::Warning=>"WARN", Self::Notice=>"NOTICE",
            Self::Info=>"INFO", Self::Debug=>"DEBUG", Self::Unknown=>"????",
        }
    }
    pub fn severity(&self) -> u8 {
        match self {
            Self::Emergency=>0, Self::Alert=>1, Self::Critical=>2, Self::Error=>3,
            Self::Warning=>4, Self::Notice=>5, Self::Info=>6, Self::Debug=>7,
            Self::Unknown=>8,
        }
    }
}

/// Filter criteria.
#[derive(Debug, Clone, Default)]
pub struct LogFilter {
    pub min_level: Option<LogLevel>,
    pub units: Vec<String>,
    pub grep: Option<String>,
    pub pattern: Option<String>,
    pub since: Option<u64>,
    pub until: Option<u64>,
}
impl LogFilter {
    pub fn matches(&self, e: &LogEntry) -> bool {
        if let Some(min) = self.min_level {
            if e.level.severity() > min.severity() { return false; }
        }
        if !self.units.is_empty() && !self.units.iter().any(|u| e.unit == *u) {
            return false;
        }
        if let Some(g) = &self.grep {
            if !e.message.to_lowercase().contains(&g.to_lowercase()) { return false; }
        }
        if let Some(p) = &self.pattern {
            if !glob_match(p, &e.message) && !glob_match(p, &e.unit) { return false; }
        }
        if let Some(s) = self.since { if e.ts < s { return false; } }
        if let Some(u) = self.until { if e.ts > u { return false; } }
        true
    }
}

/// Simple glob matcher: `*` any seq, `?` single char, case-insensitive.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.to_lowercase().chars().collect();
    let t: Vec<char> = text.to_lowercase().chars().collect();
    glob_rec(&p, &t)
}
fn glob_rec(p: &[char], t: &[char]) -> bool {
    if p.is_empty() { return t.is_empty(); }
    match p[0] {
        '*' => {
            let mut i = 0;
            while i < p.len() && p[i] == '*' { i += 1; }
            for skip in 0..=t.len() {
                if glob_rec(&p[i..], &t[skip..]) { return true; }
            }
            false
        }
        '?' => !t.is_empty() && glob_rec(&p[1..], &t[1..]),
        c => !t.is_empty() && t[0] == c && glob_rec(&p[1..], &t[1..]),
    }
}

/// Parse a log line: ISO journald, BSD syslog, key=value journal export,
/// or plain text.
pub fn parse_syslog(line: &str) -> Option<LogEntry> {
    let raw = line.to_string();
    let trimmed = line.trim();
    if trimmed.is_empty() { return None; }
    if let Some(e) = parse_iso(trimmed, &raw) { return Some(e); }
    if let Some(e) = parse_bsd(trimmed, &raw) { return Some(e); }
    if trimmed.contains("PRIORITY=") || trimmed.contains("SYSLOG_IDENTIFIER=") {
        return Some(parse_structured(trimmed, &raw));
    }
    Some(LogEntry { ts: 0, level: LogLevel::Unknown, unit: String::new(),
        message: trimmed.to_string(), raw })
}

fn parse_iso(line: &str, raw: &str) -> Option<LogEntry> {
    let bytes = line.as_bytes();
    if bytes.len() < 20 || bytes[4] != b'-' || bytes[7] != b'-' || bytes[10] != b'T' {
        return None;
    }
    let y: u64 = line.get(0..4)?.parse().ok()?;
    let mo: u64 = line.get(5..7)?.parse().ok()?;
    let d: u64 = line.get(8..10)?.parse().ok()?;
    let h: u64 = line.get(11..13)?.parse().ok()?;
    let mi: u64 = line.get(14..16)?.parse().ok()?;
    let s: u64 = line.get(17..19)?.parse().ok()?;
    let days = days_from_civil(y, mo, d);
    let ts = days * 86400 + h * 3600 + mi * 60 + s;
    // skip timestamp + timezone suffix
    let mut rest = &line[19..];
    // skip until first space after ts+tz
    if let Some(sp) = rest.find(' ') { rest = &rest[sp+1..]; }
    let rest = rest.trim_start();
    let mut parts = rest.splitn(2, ' ');
    let _host = parts.next().unwrap_or("");
    let body = parts.next().unwrap_or("");
    let (level, unit, msg) = split_unit_msg(body);
    Some(LogEntry { ts, level, unit, message: msg, raw: raw.to_string() })
}

fn parse_bsd(line: &str, raw: &str) -> Option<LogEntry> {
    let months = ["Jan","Feb","Mar","Apr","May","Jun","Jul","Aug","Sep","Oct","Nov","Dec"];
    let mon = line.get(0..3)?;
    let mi = months.iter().position(|m| *m == mon)? as u64 + 1;
    let rest = line.get(4..)?.trim_start();
    let day_end = rest.find(' ')?;
    let day: u64 = rest.get(..day_end)?.trim().parse().ok()?;
    let rest2 = rest.get(day_end..)?.trim_start();
    let time_end = rest2.find(' ')?;
    let time = rest2.get(..time_end)?;
    let mut tp = time.split(':');
    let h: u64 = tp.next()?.parse().ok()?;
    let m: u64 = tp.next()?.parse().ok()?;
    let s: u64 = tp.next()?.parse().ok()?;
    let days = days_from_civil(2024, mi, day);
    let ts = days * 86400 + h * 3600 + m * 60 + s;
    let rest3 = rest2.get(time_end..)?.trim_start();
    let mut parts = rest3.splitn(2, ' ');
    let _host = parts.next().unwrap_or("");
    let body = parts.next().unwrap_or("");
    let (level, unit, msg) = split_unit_msg(body);
    Some(LogEntry { ts, level, unit, message: msg, raw: raw.to_string() })
}

fn parse_structured(line: &str, raw: &str) -> LogEntry {
    let mut level = LogLevel::Unknown;
    let mut unit = String::new();
    let mut msg = String::new();
    let mut ts = 0u64;
    for kv in line.split_whitespace() {
        if let Some((k, v)) = kv.split_once('=') {
            match k {
                "PRIORITY" => {
                    if let Ok(p) = v.parse::<u8>() { level = LogLevel::from_syslog_pri(p); }
                }
                "SYSLOG_IDENTIFIER" | "_SYSTEMD_UNIT" | "UNIT" => {
                    unit = v.trim_matches('"').to_string();
                }
                "MESSAGE" => { msg = v.trim_matches('"').to_string(); }
                "__REALTIME_TIMESTAMP" => {
                    if let Ok(us) = v.parse::<u64>() { ts = us / 1_000_000; }
                }
                _ => {}
            }
        }
    }
    if msg.is_empty() { msg = line.to_string(); }
    LogEntry { ts, level, unit, message: msg, raw: raw.to_string() }
}

fn split_unit_msg(body: &str) -> (LogLevel, String, String) {
    let body = body.trim();
    if body.is_empty() { return (LogLevel::Unknown, String::new(), String::new()); }
    let level_words = ["EMERG","ALERT","CRIT","CRITICAL","ERR","ERROR","WARN","WARNING",
        "NOTICE","INFO","DEBUG","TRACE"];
    let mut level = LogLevel::Unknown;
    let mut rest = body;
    let first = body.split_whitespace().next().unwrap_or("");
    let fl = first.trim_end_matches(':').to_uppercase();
    if level_words.contains(&fl.as_str()) {
        level = match fl.as_str() {
            "EMERG"=>LogLevel::Emergency, "ALERT"=>LogLevel::Alert,
            "CRIT"|"CRITICAL"=>LogLevel::Critical, "ERR"|"ERROR"=>LogLevel::Error,
            "WARN"|"WARNING"=>LogLevel::Warning, "NOTICE"=>LogLevel::Notice,
            "INFO"=>LogLevel::Info, "DEBUG"|"TRACE"=>LogLevel::Debug,
            _=>LogLevel::Unknown,
        };
        rest = body[first.len()..].trim_start();
    }
    if let Some(colon) = rest.find(':') {
        let head = rest[..colon].trim();
        let msg = rest[colon+1..].trim().to_string();
        let unit = head.split('[').next().unwrap_or(head).trim().to_string();
        (level, unit, msg)
    } else {
        (level, String::new(), rest.to_string())
    }
}

fn days_from_civil(y: u64, m: u64, d: u64) -> u64 {
    let y = if m <= 2 { y - 1 } else { y } as i64;
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    (era * 146097 + doe - 719468) as u64
}

/// The viewer: holds parsed entries, supports query/pagination/stats.
#[derive(Debug, Default)]
pub struct LogViewer {
    pub entries: Vec<LogEntry>,
    pub source: String,
}

impl LogViewer {
    pub fn new() -> Self { Self::default() }

    pub fn ingest_lines<I: IntoIterator<Item = String>>(&mut self, lines: I) {
        for l in lines {
            if let Some(e) = parse_syslog(&l) {
                self.entries.push(e);
            }
        }
    }

    pub fn load_file(&mut self, path: &Path) -> Result<usize, String> {
        let txt = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
        let n = txt.lines().count();
        self.ingest_lines(txt.lines().map(|s| s.to_string()));
        self.source = path.display().to_string();
        Ok(n)
    }

    pub fn query(&self, filter: &LogFilter, offset: usize, limit: usize) -> Vec<&LogEntry> {
        self.entries.iter().filter(|e| filter.matches(e))
            .skip(offset).take(limit).collect()
    }

    pub fn count(&self, filter: &LogFilter) -> usize {
        self.entries.iter().filter(|e| filter.matches(e)).count()
    }

    pub fn sorted(&self) -> Vec<&LogEntry> {
        let mut v: Vec<&LogEntry> = self.entries.iter().collect();
        v.sort_by_key(|e| e.ts);
        v
    }

    pub fn stats(&self) -> (BTreeMap<String, usize>, BTreeMap<String, usize>) {
        let mut by_level: BTreeMap<String, usize> = BTreeMap::new();
        let mut by_unit: BTreeMap<String, usize> = BTreeMap::new();
        for e in &self.entries {
            *by_level.entry(e.level.name().to_string()).or_insert(0) += 1;
            if !e.unit.is_empty() {
                *by_unit.entry(e.unit.clone()).or_insert(0) += 1;
            }
        }
        (by_level, by_unit)
    }

    pub fn time_range(&self) -> Option<(u64, u64)> {
        let mut min = u64::MAX;
        let mut max = 0u64;
        let mut any = false;
        for e in &self.entries {
            if e.ts > 0 {
                any = true;
                if e.ts < min { min = e.ts; }
                if e.ts > max { max = e.ts; }
            }
        }
        if any { Some((min, max)) } else { None }
    }

    pub fn since(&self, since_ts: u64) -> Vec<&LogEntry> {
        self.entries.iter().filter(|e| e.ts > since_ts).collect()
    }

    pub fn render(&self, entries: &[&LogEntry]) -> String {
        let mut out = String::new();
        for e in entries {
            out.push_str(&format!("[{}] {:<7} {:<20} {}\n",
                fmt_ts(e.ts), e.level.name(), e.unit, e.message));
        }
        out
    }
}

fn fmt_ts(ts: u64) -> String {
    if ts == 0 { return "--:--:--".into(); }
    let days = ts / 86400;
    let rem = ts % 86400;
    let h = rem / 3600;
    let m = (rem % 3600) / 60;
    let s = rem % 60;
    let dow = ["Thu","Fri","Sat","Sun","Mon","Tue","Wed"][(days % 7) as usize];
    let (_y, mo, d) = civil_from_days(days);
    format!("{} {:02} {:02} {:02}:{:02}:{:02}", dow, mo, d, h, m, s)
}

fn civil_from_days(z: u64) -> (u64, u64, u64) {
    let z = z as i64 + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe/1460 + doe/36524 - doe/146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365*yoe + yoe/4 - yoe/100);
    let mp = (5*doy + 2)/153;
    let d = doy - (153*mp+2)/5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y } as u64, m as u64, d as u64)
}

/// Persisted viewer state (bookmarks, last position).
#[derive(serde::Serialize, serde::Deserialize, Default)]
pub struct ViewerState {
    pub last_position: u64,
    pub bookmarks: Vec<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_iso_journal() {
        let line = "2024-01-15T10:30:45+00:00 myhost sshd[1234]: Failed password for root";
        let e = parse_syslog(line).unwrap();
        assert!(e.ts > 0);
        assert_eq!(e.unit, "sshd");
        assert!(e.message.contains("Failed password"));
    }

    #[test]
    fn parse_bsd_syslog() {
        let line = "Jan 15 10:30:45 myhost cron[999]: (root) CMD (backup.sh)";
        let e = parse_syslog(line).unwrap();
        assert!(e.ts > 0);
        assert_eq!(e.unit, "cron");
        assert!(e.message.contains("backup.sh"));
    }

    #[test]
    fn parse_structured_journal() {
        let line = "PRIORITY=3 SYSLOG_IDENTIFIER=kernel __REALTIME_TIMESTAMP=1705314645000000 MESSAGE=\"oom-kill: process killed\"";
        let e = parse_syslog(line).unwrap();
        assert_eq!(e.level, LogLevel::Error);
        assert_eq!(e.unit, "kernel");
        assert_eq!(e.ts, 1705314645);
        assert!(e.message.contains("oom-kill"));
    }

    #[test]
    fn parse_plain_line() {
        let e = parse_syslog("just some text").unwrap();
        assert_eq!(e.level, LogLevel::Unknown);
        assert_eq!(e.message, "just some text");
        assert_eq!(e.ts, 0);
    }

    #[test]
    fn glob_matcher() {
        assert!(glob_match("error*", "error: disk full"));
        assert!(glob_match("*failed*", "login failed"));
        assert!(glob_match("err?r", "error"));
        assert!(!glob_match("warn*", "error: x"));
        assert!(glob_match("*", "anything"));
    }

    #[test]
    fn filter_by_level() {
        let mut v = LogViewer::new();
        v.ingest_lines(vec![
            "2024-01-15T10:00:00+00:00 h ERR unit: err msg".to_string(),
            "2024-01-15T10:01:00+00:00 h INFO unit: info msg".to_string(),
            "2024-01-15T10:02:00+00:00 h DEBUG unit: dbg msg".to_string(),
        ]);
        let f = LogFilter { min_level: Some(LogLevel::Warning), ..Default::default() };
        let got = v.query(&f, 0, 100);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].level, LogLevel::Error);
    }

    #[test]
    fn filter_by_unit_and_grep() {
        let mut v = LogViewer::new();
        v.ingest_lines(vec![
            "2024-01-15T10:00:00+00:00 h INFO sshd: accepted key".to_string(),
            "2024-01-15T10:01:00+00:00 h INFO cron: ran job".to_string(),
            "2024-01-15T10:02:00+00:00 h ERR sshd: failed auth".to_string(),
        ]);
        let f = LogFilter { units: vec!["sshd".into()], grep: Some("fail".into()), ..Default::default() };
        let got = v.query(&f, 0, 100);
        assert_eq!(got.len(), 1);
        assert!(got[0].message.contains("failed"));
    }

    #[test]
    fn pagination() {
        let mut v = LogViewer::new();
        for i in 0..10 {
            v.ingest_lines(vec![format!("2024-01-15T10:{:02}:00+00:00 h INFO u: msg {}", i, i)]);
        }
        let f = LogFilter::default();
        assert_eq!(v.count(&f), 10);
        let page1 = v.query(&f, 0, 3);
        let page2 = v.query(&f, 3, 3);
        assert_eq!(page1.len(), 3);
        assert_eq!(page2.len(), 3);
        assert_ne!(page1[0].raw, page2[0].raw);
    }

    #[test]
    fn stats_and_range() {
        let mut v = LogViewer::new();
        v.ingest_lines(vec![
            "2024-01-15T10:00:00+00:00 h ERR sshd: e1".to_string(),
            "2024-01-15T10:05:00+00:00 h WARN cron: w1".to_string(),
            "2024-01-15T10:10:00+00:00 h ERR sshd: e2".to_string(),
        ]);
        let (lv, uv) = v.stats();
        assert_eq!(lv.get("ERR"), Some(&2));
        assert_eq!(lv.get("WARN"), Some(&1));
        assert_eq!(uv.get("sshd"), Some(&2));
        let (min, max) = v.time_range().unwrap();
        assert!(max > min);
    }

    #[test]
    fn sorted_and_since() {
        let mut v = LogViewer::new();
        v.ingest_lines(vec![
            "2024-01-15T10:10:00+00:00 h INFO u: later".to_string(),
            "2024-01-15T10:00:00+00:00 h INFO u: earlier".to_string(),
        ]);
        let s = v.sorted();
        assert!(s[0].message.contains("earlier"));
        let new = v.since(s[0].ts);
        assert_eq!(new.len(), 1);
        assert!(new[0].message.contains("later"));
    }

    #[test]
    fn render_format() {
        let mut v = LogViewer::new();
        v.ingest_lines(vec!["2024-01-15T10:00:00+00:00 h ERR sshd: bad".to_string()]);
        let out = v.render(&v.sorted());
        assert!(out.contains("ERR"));
        assert!(out.contains("sshd"));
        assert!(out.contains("bad"));
    }

    #[test]
    fn empty_source() {
        let v = LogViewer::new();
        assert_eq!(v.count(&LogFilter::default()), 0);
        assert!(v.time_range().is_none());
    }
}
