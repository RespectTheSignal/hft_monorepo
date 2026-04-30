use std::collections::VecDeque;
use std::env;
use std::path::{Path, PathBuf};

use anyhow::Context;
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

#[derive(Debug, Clone)]
struct Config {
    bind: String,
    project: PathBuf,
}

#[derive(Debug, Serialize)]
struct Snapshot {
    now: String,
    processes: Vec<ProcessStatus>,
    feed: FeedStatus,
    recent_signals: Vec<SignalRow>,
    recent_exec: Vec<ExecRow>,
    trades: TradeSummary,
}

#[derive(Debug, Serialize)]
struct ProcessStatus {
    name: String,
    pid: Option<u32>,
    alive: bool,
    age_s: Option<u64>,
}

#[derive(Debug, Default, Serialize)]
struct FeedStatus {
    binance: Option<u64>,
    gate: Option<u64>,
    flipster: Option<u64>,
}

#[derive(Debug, Serialize)]
struct SignalRow {
    ts: String,
    event: String,
    base: String,
    side: String,
    detail: String,
}

#[derive(Debug, Serialize)]
struct ExecRow {
    ts: String,
    event: String,
    base: String,
    detail: String,
}

#[derive(Debug, Default, Serialize)]
struct TradeSummary {
    n: usize,
    wins: usize,
    losses: usize,
    net: f64,
    gross: f64,
    fees: f64,
    last: Vec<Value>,
    by_base: Vec<BasePnl>,
}

#[derive(Debug, Default, Clone, Serialize)]
struct BasePnl {
    base: String,
    n: usize,
    net: f64,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cfg = Config {
        bind: env::var("DASHBOARD_BIND").unwrap_or_else(|_| "0.0.0.0:8090".to_string()),
        project: env::var("FLIPSTER_KATTPISH_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| env::current_dir().unwrap_or_else(|_| PathBuf::from("."))),
    };
    let listener = TcpListener::bind(&cfg.bind)
        .await
        .with_context(|| format!("bind {}", cfg.bind))?;
    println!("[live_dashboard] http://{}", cfg.bind);
    loop {
        let (stream, _) = listener.accept().await?;
        let cfg = cfg.clone();
        tokio::spawn(async move {
            if let Err(e) = handle(stream, cfg).await {
                eprintln!("[live_dashboard] request error: {e:#}");
            }
        });
    }
}

async fn handle(mut stream: TcpStream, cfg: Config) -> anyhow::Result<()> {
    let mut buf = [0u8; 2048];
    let n = stream.read(&mut buf).await?;
    let req = String::from_utf8_lossy(&buf[..n]);
    let path = req
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .unwrap_or("/");
    match path {
        "/" => respond(&mut stream, "text/html; charset=utf-8", HTML.as_bytes()).await?,
        "/api/status" => {
            let body = serde_json::to_vec(&snapshot(&cfg.project))?;
            respond(&mut stream, "application/json; charset=utf-8", &body).await?;
        }
        _ => respond_status(&mut stream, 404, "not found").await?,
    }
    Ok(())
}

async fn respond(stream: &mut TcpStream, content_type: &str, body: &[u8]) -> anyhow::Result<()> {
    let head = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: {content_type}\r\ncache-control: no-store\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body).await?;
    Ok(())
}

async fn respond_status(stream: &mut TcpStream, code: u16, msg: &str) -> anyhow::Result<()> {
    let body = msg.as_bytes();
    let head = format!(
        "HTTP/1.1 {code} {msg}\r\ncontent-type: text/plain\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body).await?;
    Ok(())
}

fn snapshot(project: &Path) -> Snapshot {
    let collector_log = read_tail(&project.join("logs/collector.log"), 512_000);
    let executor_log = read_tail(&project.join("logs/executor_v44.log"), 512_000);
    Snapshot {
        now: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        processes: vec![
            proc_status("collector", &project.join("run/collector.pid")),
            proc_status("executor_gl", &project.join("run/executor_gl.pid")),
        ],
        feed: parse_feed(&collector_log),
        recent_signals: parse_signals(&collector_log, 50),
        recent_exec: parse_exec(&executor_log, 80),
        trades: parse_trades(&project.join("logs/binance_lead_live_v44.jsonl")),
    }
}

fn proc_status(name: &str, pid_path: &Path) -> ProcessStatus {
    let pid = std::fs::read_to_string(pid_path)
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok());
    let (alive, age_s) = pid
        .map(|p| {
            let stat = PathBuf::from(format!("/proc/{p}"));
            let alive = stat.exists();
            let age_s = if alive {
                std::fs::metadata(stat)
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.elapsed().ok())
                    .map(|d| d.as_secs())
            } else {
                None
            };
            (alive, age_s)
        })
        .unwrap_or((false, None));
    ProcessStatus {
        name: name.to_string(),
        pid,
        alive,
        age_s,
    }
}

fn read_tail(path: &Path, max_bytes: usize) -> String {
    let Ok(bytes) = std::fs::read(path) else {
        return String::new();
    };
    let start = bytes.len().saturating_sub(max_bytes);
    String::from_utf8_lossy(&bytes[start..]).to_string()
}

fn parse_feed(log: &str) -> FeedStatus {
    let mut out = FeedStatus::default();
    for line in log.lines() {
        if !line.contains("cumulative recv") {
            continue;
        }
        let ex = field(line, "exchange").unwrap_or_default();
        let count = field(line, "count").and_then(|s| s.parse::<u64>().ok());
        match ex.as_str() {
            "binance" => out.binance = count,
            "gate" => out.gate = count,
            "flipster" => out.flipster = count,
            _ => {}
        }
    }
    out
}

fn parse_signals(log: &str, limit: usize) -> Vec<SignalRow> {
    let mut rows = VecDeque::new();
    for line in log.lines() {
        if !(line.contains("[gate_lead] OPEN")
            || line.contains("[gate_lead] CLOSE")
            || line.contains("[coord] skip"))
        {
            continue;
        }
        let ts = ts_prefix(line);
        let event = if line.contains("[gate_lead] OPEN") {
            "OPEN"
        } else if line.contains("[gate_lead] CLOSE") {
            "CLOSE"
        } else {
            "SKIP"
        };
        let base = field(line, "base").unwrap_or_default();
        let side = field(line, "side").unwrap_or_default();
        let detail = if event == "SKIP" {
            field(line, "reason").unwrap_or_default()
        } else {
            ["gate_move_bp", "f_entry", "f_exit", "net_bp", "hold_ms", "reason"]
                .iter()
                .filter_map(|k| field(line, k).map(|v| format!("{k}={v}")))
                .collect::<Vec<_>>()
                .join(" ")
        };
        push_limit(
            &mut rows,
            SignalRow {
                ts,
                event: event.to_string(),
                base,
                side,
                detail,
            },
            limit,
        );
    }
    rows.into_iter().rev().collect()
}

fn parse_exec(log: &str, limit: usize) -> Vec<ExecRow> {
    let mut rows = VecDeque::new();
    for line in log.lines() {
        let event = if line.contains("[ENTRY]") {
            "ENTRY"
        } else if line.contains("[BBO-RECHECK]") {
            "BBO"
        } else if line.contains("[TOPBOOK-SIZE]") {
            "SIZE"
        } else if line.contains("[FLIPSTER-") || line.contains("[MARKET-ENTRY]") {
            "ORDER"
        } else if line.contains("[SLIP-IN]") {
            "SLIP"
        } else if line.contains("[PNL]") {
            "PNL"
        } else {
            continue;
        };
        let base = parse_entry_base(line).unwrap_or_default();
        push_limit(
            &mut rows,
            ExecRow {
                ts: ts_prefix(line),
                event: event.to_string(),
                base,
                detail: strip_ansi(line).trim().to_string(),
            },
            limit,
        );
    }
    rows.into_iter().rev().collect()
}

fn parse_trades(path: &Path) -> TradeSummary {
    let Ok(txt) = std::fs::read_to_string(path) else {
        return TradeSummary::default();
    };
    let mut summary = TradeSummary::default();
    let mut last = VecDeque::new();
    let mut by_base: std::collections::BTreeMap<String, BasePnl> = Default::default();
    for line in txt.lines().filter(|l| !l.trim().is_empty()) {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let net = v["net_after_fees_usd"].as_f64().unwrap_or(0.0);
        let gross = v["net_pnl_usd"].as_f64().unwrap_or(0.0);
        let fee = v["approx_fee_usd"].as_f64().unwrap_or(0.0);
        summary.n += 1;
        summary.net += net;
        summary.gross += gross;
        summary.fees += fee;
        if net > 0.0 {
            summary.wins += 1;
        } else if net < 0.0 {
            summary.losses += 1;
        }
        let base = v["base"].as_str().unwrap_or("").to_string();
        let e = by_base.entry(base.clone()).or_insert(BasePnl {
            base,
            n: 0,
            net: 0.0,
        });
        e.n += 1;
        e.net += net;
        push_limit(&mut last, v, 50);
    }
    summary.last = last.into_iter().rev().collect();
    let mut bases = by_base.into_values().collect::<Vec<_>>();
    bases.sort_by(|a, b| a.net.partial_cmp(&b.net).unwrap_or(std::cmp::Ordering::Equal));
    summary.by_base = bases;
    summary.net = round6(summary.net);
    summary.gross = round6(summary.gross);
    summary.fees = round6(summary.fees);
    summary
}

fn push_limit<T>(q: &mut VecDeque<T>, v: T, limit: usize) {
    q.push_back(v);
    while q.len() > limit {
        q.pop_front();
    }
}

fn ts_prefix(line: &str) -> String {
    let clean = strip_ansi(line);
    clean
        .split_whitespace()
        .next()
        .unwrap_or("")
        .trim()
        .to_string()
}

fn field(line: &str, key: &str) -> Option<String> {
    let clean = strip_ansi(line);
    let pat = format!("{key}=");
    let idx = clean.find(&pat)? + pat.len();
    let rest = &clean[idx..];
    let raw = rest.split_whitespace().next()?.trim();
    Some(raw.trim_matches('"').trim_matches(',').to_string())
}

fn parse_entry_base(line: &str) -> Option<String> {
    let clean = strip_ansi(line);
    if let Some(i) = clean.find("[ENTRY]") {
        return clean[i + 7..].split('|').next().map(|s| s.trim().to_string());
    }
    None
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            for x in chars.by_ref() {
                if x.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn round6(v: f64) -> f64 {
    (v * 1_000_000.0).round() / 1_000_000.0
}

#[allow(dead_code)]
fn parse_iso(ts: &str) -> Option<DateTime<Utc>> {
    ts.replace('Z', "+00:00")
        .parse::<DateTime<chrono::FixedOffset>>()
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

const HTML: &str = r#"<!doctype html>
<html>
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>GL Live Dashboard</title>
<style>
body{margin:0;background:#0e1116;color:#d6dee8;font-family:Inter,ui-sans-serif,system-ui,-apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif;font-size:13px}
header{height:52px;display:flex;align-items:center;justify-content:space-between;padding:0 18px;border-bottom:1px solid #263241;background:#121821}
h1{font-size:18px;margin:0;font-weight:650;letter-spacing:0}
.muted{color:#8ea0b5}.ok{color:#36d399}.bad{color:#ff6b6b}.warn{color:#f6c85f}
main{padding:14px 18px;display:grid;grid-template-columns:1.1fr .9fr;gap:14px}
.band{background:#151c26;border:1px solid #263241;border-radius:8px;padding:12px}
.cards{display:grid;grid-template-columns:repeat(5,minmax(0,1fr));gap:10px}
.card{background:#101720;border:1px solid #263241;border-radius:6px;padding:10px;min-height:58px}
.label{font-size:11px;text-transform:uppercase;color:#8ea0b5}.value{font-size:22px;margin-top:4px;font-weight:700}
table{width:100%;border-collapse:collapse}th,td{padding:7px 8px;border-bottom:1px solid #263241;text-align:left;vertical-align:top}
th{font-size:11px;text-transform:uppercase;color:#8ea0b5;font-weight:600}td.num{text-align:right;font-variant-numeric:tabular-nums}
.scroll{max-height:420px;overflow:auto}.wide{grid-column:1 / -1}.pill{display:inline-block;padding:2px 7px;border-radius:999px;background:#263241;color:#c9d6e4;font-size:12px}
@media(max-width:1100px){main{grid-template-columns:1fr}.cards{grid-template-columns:repeat(2,minmax(0,1fr))}}
</style>
</head>
<body>
<header><h1>GL Live Dashboard</h1><div class="muted" id="now">loading</div></header>
<main>
<section class="band wide"><div class="cards" id="cards"></div></section>
<section class="band"><h2>Signals</h2><div class="scroll"><table id="signals"></table></div></section>
<section class="band"><h2>Execution</h2><div class="scroll"><table id="exec"></table></div></section>
<section class="band"><h2>Recent Fills</h2><div class="scroll"><table id="fills"></table></div></section>
<section class="band"><h2>By Base</h2><div class="scroll"><table id="bases"></table></div></section>
</main>
<script>
const fmt = n => Number(n||0).toFixed(4);
const cls = v => v > 0 ? 'ok' : v < 0 ? 'bad' : '';
function cell(v,c=''){return `<td class="${c}">${v??''}</td>`}
function proc(data,name){return (data.processes||[]).find(p=>p.name===name)||{}}
async function load(){
  const r = await fetch('/api/status',{cache:'no-store'});
  const d = await r.json();
  document.getElementById('now').textContent = d.now;
  const col = proc(d,'collector'), gl = proc(d,'executor_gl');
  const wr = d.trades.n ? (d.trades.wins/d.trades.n*100).toFixed(1)+'%' : '0.0%';
  const cards = [
    ['Collector', col.alive ? 'ALIVE' : 'DEAD', col.alive?'ok':'bad'],
    ['GL Executor', gl.alive ? 'ALIVE' : 'DEAD', gl.alive?'ok':'bad'],
    ['Feed', `B ${d.feed.binance||0} / G ${d.feed.gate||0} / F ${d.feed.flipster||0}`, ''],
    ['Trades / WR', `${d.trades.n} / ${wr}`, ''],
    ['Net PnL', fmt(d.trades.net), cls(d.trades.net)]
  ];
  document.getElementById('cards').innerHTML = cards.map(x=>`<div class="card"><div class="label">${x[0]}</div><div class="value ${x[2]}">${x[1]}</div></div>`).join('');
  document.getElementById('signals').innerHTML = '<tr><th>Time</th><th>Event</th><th>Base</th><th>Side</th><th>Detail</th></tr>' +
    (d.recent_signals||[]).map(x=>`<tr>${cell(x.ts)}${cell(`<span class="pill">${x.event}</span>`)}${cell(x.base)}${cell(x.side)}${cell(x.detail)}</tr>`).join('');
  document.getElementById('exec').innerHTML = '<tr><th>Time</th><th>Event</th><th>Base</th><th>Detail</th></tr>' +
    (d.recent_exec||[]).map(x=>`<tr>${cell(x.ts)}${cell(`<span class="pill">${x.event}</span>`)}${cell(x.base)}${cell(x.detail)}</tr>`).join('');
  document.getElementById('fills').innerHTML = '<tr><th>Close</th><th>Base</th><th>Side</th><th>Size</th><th>Entry</th><th>Exit</th><th>Slip In/Out</th><th>RTT</th><th>Net</th></tr>' +
    (d.trades.last||[]).map(x=>`<tr>${cell(x.ts_close)}${cell(x.base)}${cell(x.flipster_side)}${cell(x.size_usd,'num')}${cell(x.flipster_entry,'num')}${cell(x.flipster_exit,'num')}${cell(`${x.f_entry_slip_bp??''}/${x.f_exit_slip_bp??''}`,'num')}${cell(`${x.f_entry_order_rtt_ms??''}/${x.f_exit_order_rtt_ms??''}`,'num')}${cell(fmt(x.net_after_fees_usd),'num '+cls(x.net_after_fees_usd))}</tr>`).join('');
  document.getElementById('bases').innerHTML = '<tr><th>Base</th><th>N</th><th>Net</th></tr>' +
    (d.trades.by_base||[]).map(x=>`<tr>${cell(x.base)}${cell(x.n,'num')}${cell(fmt(x.net),'num '+cls(x.net))}</tr>`).join('');
}
load(); setInterval(load, 2000);
</script>
</body>
</html>
"#;
