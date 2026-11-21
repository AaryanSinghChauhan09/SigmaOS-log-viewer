//! SigmaOS-log-viewer — journal/log viewer CLI.
//!
//! Usage:
//!   sigmaos-logview <file> [--level ERR|WARN|INFO|DEBUG] [--unit X]
//!   sigmaos-logview <file> --grep PAT | --pattern GLOB | --stats
//!   sigmaos-logview journal [--lines N]   (reads journalctl if available)
//!
//! Reads files directly; `journal` shells to journalctl -o export when present.

use sigmaos_log_viewer::{LogFilter, LogLevel, LogViewer};
use std::path::Path;
use std::process::exit;

fn usage() -> ! {
    eprintln!("usage: sigmaos-logview <file|journal> [--level L] [--unit U] [--grep G] [--pattern P] [--since TS] [--until TS] [--stats] [--lines N]");
    exit(2);
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() { usage(); }
    let source = args[0].clone();
    let rest = &args[1..];

    let mut filter = LogFilter::default();
    let mut show_stats = false;
    let mut limit = 200usize;
    let mut i = 0;
    while i < rest.len() {
        match rest[i].as_str() {
            "--level" => {
                i += 1;
                let l = rest.get(i).map(|s| s.as_str()).unwrap_or("");
                filter.min_level = Some(match l.to_uppercase().as_str() {
                    "EMERG"=>LogLevel::Emergency, "ALERT"=>LogLevel::Alert,
                    "CRIT"|"CRITICAL"=>LogLevel::Critical, "ERR"|"ERROR"=>LogLevel::Error,
                    "WARN"|"WARNING"=>LogLevel::Warning, "NOTICE"=>LogLevel::Notice,
                    "INFO"=>LogLevel::Info, "DEBUG"=>LogLevel::Debug,
                    _ => { eprintln!("bad level"); exit(2); }
                });
            }
            "--unit" => { i += 1; filter.units.push(rest.get(i).cloned().unwrap_or_default()); }
            "--grep" => { i += 1; filter.grep = rest.get(i).cloned(); }
            "--pattern" => { i += 1; filter.pattern = rest.get(i).cloned(); }
            "--since" => { i += 1; filter.since = rest.get(i).and_then(|s| s.parse().ok()); }
            "--until" => { i += 1; filter.until = rest.get(i).and_then(|s| s.parse().ok()); }
            "--stats" => show_stats = true,
            "--lines" => { i += 1; limit = rest.get(i).and_then(|s| s.parse().ok()).unwrap_or(200); }
            other => { eprintln!("unknown arg: {}", other); usage(); }
        }
        i += 1;
    }

    let mut v = LogViewer::new();
    let load_result: Result<usize, String> = if source == "journal" {
        // journalctl export format → structured parser
        match std::process::Command::new("journalctl")
            .args(["-o","export","--no-pager","-n", &limit.to_string()]).output() {
            Ok(o) if o.status.success() => {
                let txt = String::from_utf8_lossy(&o.stdout).to_string();
                let n = txt.lines().count();
                v.ingest_lines(txt.lines().map(|s| s.to_string()));
                v.source = "journal".into();
                Ok(n)
            }
            Ok(o) => Err(format!("journalctl exit {}: {}", o.status,
                String::from_utf8_lossy(&o.stderr))),
            Err(e) => Err(format!("journalctl unavailable: {}", e)),
        }
    } else {
        v.load_file(Path::new(&source))
    };

    match load_result {
        Ok(n) => eprintln!("loaded {} lines from {}", n, source),
        Err(e) => { eprintln!("sigmaos-logview: {}", e); exit(1); }
    }

    if show_stats {
        let (lv, uv) = v.stats();
        println!("== by level ==");
        for (k, c) in &lv { println!("  {:<7} {}", k, c); }
        println!("== by unit ==");
        for (k, c) in uv.iter().take(20) { println!("  {:<20} {}", k, c); }
        if let Some((min, max)) = v.time_range() {
            println!("time range: {} → {}", min, max);
        }
        return;
    }

    let total = v.count(&filter);
    eprintln!("{} matching entries", total);
    let entries = v.query(&filter, 0, limit);
    print!("{}", v.render(&entries));
}
