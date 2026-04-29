#!/usr/bin/env python3
"""Web dashboard for flipster_latency_pick bot.

Runs a small HTTP server that serves a live HTML page + JSON API.
Parses the bot's log, applies Flipster VIP fees (maker 0.15bp / taker 0.425bp),
and shows the TRUE net PnL you will settle at once rebates credit.

Usage:
    python3 scripts/dashboard_web.py                 # port 8080
    python3 scripts/dashboard_web.py --port 8081

Then open: http://100.115.233.110:<port>/
"""
from __future__ import annotations
import argparse, json, re, time
from collections import defaultdict
from datetime import datetime, timezone, timedelta
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

MAKER_FEE_BP = 0.15
TAKER_FEE_BP = 0.425
GROSS_MAKER_BP = 2.5      # balance-visible fee before rebate
GROSS_TAKER_BP = 4.25
DEFAULT_LOG = Path(__file__).resolve().parent.parent / "logs" / "latency_pick.log"

KST = timezone(timedelta(hours=9))


def fee_bp_rt(close_kind: str) -> float:
    """Round-trip fee in bp. Entry is always maker; close depends on kind."""
    if close_kind in ("MAKER", "BRACKET_TP"):
        return MAKER_FEE_BP + MAKER_FEE_BP
    return MAKER_FEE_BP + TAKER_FEE_BP


def gross_fee_bp_rt(close_kind: str) -> float:
    """Balance-visible fee (pre-rebate) for showing 'what exchange shows now'."""
    if close_kind in ("MAKER", "BRACKET_TP"):
        return GROSS_MAKER_BP + GROSS_MAKER_BP
    return GROSS_MAKER_BP + GROSS_TAKER_BP


def parse_log(log_path: Path):
    if not log_path.exists():
        return {"error": f"log not found: {log_path}"}
    txt = log_path.read_text()

    m = re.search(r"amount=\$([\d.]+)", txt)
    amount_usd = float(m.group(1)) if m else 5.0

    closes = []
    ts_line_re = re.compile(r"\[(\d\d:\d\d:\d\d)\]")
    last_ts = None
    for line in txt.splitlines():
        # Track the last HH:MM:SS we saw (bot logs SIG/STATUS with time)
        t = ts_line_re.match(line)
        if t:
            last_ts = t.group(1)
        m1 = re.match(
            r"  \[CLOSE:(BRACKET_TP)\] (\S+) \w+ tp=[\d.]+ "
            r"move=([+-][\d.]+)bp hold=(\d+)ms pnl=\$(-?[\d.]+)", line)
        if m1:
            kind, sym, move, hold, pnl = m1.groups()
            closes.append({"kind": kind, "reason": "bracket_tp", "sym": sym,
                           "move_bp": float(move), "hold_ms": int(hold),
                           "gross_pnl": float(pnl), "ts": last_ts})
            continue
        m2 = re.match(
            r"  \[CLOSE:(\w+)\] (\S+) \w+ reason=(\w+)\S* .*?"
            r"move=([+-][\d.]+)bp hold=(\d+)ms pnl=\$(-?[\d.]+)", line)
        if m2:
            kind, sym, reason, move, hold, pnl = m2.groups()
            closes.append({"kind": kind, "reason": reason, "sym": sym,
                           "move_bp": float(move), "hold_ms": int(hold),
                           "gross_pnl": float(pnl), "ts": last_ts})

    status = None
    for m in re.finditer(r"\[status t=(\d+)s\] sig=(\d+) open=(\d+) close=(\d+) "
                         r"flip=\d+ skip=(\d+) open_pos=(\d+) tracker=(-?\d+) "
                         r"cum_pnl=\$(-?[\d.]+)", txt):
        status = {"t_s": int(m.group(1)), "sig": int(m.group(2)),
                  "open": int(m.group(3)), "close": int(m.group(4)),
                  "skip": int(m.group(5)), "open_pos": int(m.group(6)),
                  "tracker": int(m.group(7)), "cum_pnl": float(m.group(8))}

    extras = {
        "cancel_converged": len(re.findall(r"\[ENTRY_CANCEL:converged", txt)),
        "cancel_timeout":   len(re.findall(r"\[ENTRY_CANCEL:timeout", txt)),
        "late_fills":       len(re.findall(r"\[LATE_FILL\]", txt)),
    }

    # Aggregate per reason
    by_reason = defaultdict(lambda: {"n":0, "wins":0, "move_sum":0.0,
                                      "gross":0.0, "fees":0.0, "gross_fees":0.0})
    for c in closes:
        s = by_reason[c["reason"]]
        s["n"] += 1
        if c["gross_pnl"] > 0: s["wins"] += 1
        s["move_sum"] += c["move_bp"]
        s["gross"] += c["gross_pnl"]
        s["fees"] += amount_usd * fee_bp_rt(c["kind"]) / 1e4
        s["gross_fees"] += amount_usd * gross_fee_bp_rt(c["kind"]) / 1e4

    reasons = []
    tot = {"n":0, "wins":0, "gross":0.0, "fees":0.0, "gross_fees":0.0}
    for r, s in by_reason.items():
        tot["n"] += s["n"]; tot["wins"] += s["wins"]
        tot["gross"] += s["gross"]; tot["fees"] += s["fees"]
        tot["gross_fees"] += s["gross_fees"]
        wr = s["wins"]/s["n"]*100 if s["n"] else 0
        reasons.append({
            "reason": r, "n": s["n"], "wr": round(wr, 1),
            "avg_move_bp": round(s["move_sum"]/s["n"], 2),
            "gross": round(s["gross"], 4),
            "fees": round(s["fees"], 4),
            "net":  round(s["gross"] - s["fees"], 4),
        })
    order = {"bracket_tp":0, "tp_fallback":1, "max_hold":2, "stop":3, "hard_stop":4}
    reasons.sort(key=lambda x: order.get(x["reason"], 99))

    # Per-sym net
    sym_net = defaultdict(lambda: {"n":0, "net":0.0})
    for c in closes:
        fee = amount_usd * fee_bp_rt(c["kind"]) / 1e4
        sym_net[c["sym"]]["n"] += 1
        sym_net[c["sym"]]["net"] += c["gross_pnl"] - fee
    by_sym = [{"sym": s, "n": v["n"], "net": round(v["net"], 4)}
              for s, v in sym_net.items()]
    by_sym.sort(key=lambda x: x["net"])

    # Cumulative net PnL time series (index-based, since times are HH:MM:SS only)
    cum_series = []
    running = 0.0
    for i, c in enumerate(closes):
        fee = amount_usd * fee_bp_rt(c["kind"]) / 1e4
        running += c["gross_pnl"] - fee
        cum_series.append({"i": i+1, "ts": c["ts"], "net": round(running, 4)})

    totals = {
        "n": tot["n"],
        "wr": round(tot["wins"]/tot["n"]*100, 1) if tot["n"] else 0,
        "gross": round(tot["gross"], 4),
        "fees_net": round(tot["fees"], 4),
        "fees_gross": round(tot["gross_fees"], 4),
        "net_settled": round(tot["gross"] - tot["fees"], 4),
        "balance_view": round(tot["gross"] - tot["gross_fees"], 4),
        "pending_rebate": round(tot["gross_fees"] - tot["fees"], 4),
    }

    return {
        "ok": True,
        "now": datetime.now(KST).strftime("%Y-%m-%d %H:%M:%S KST"),
        "amount_usd": amount_usd,
        "fees": {"maker_bp": MAKER_FEE_BP, "taker_bp": TAKER_FEE_BP},
        "status": status or {},
        "extras": extras,
        "totals": totals,
        "reasons": reasons,
        "winners": [s for s in reversed(by_sym) if s["net"] > 0][:5],
        "losers":  [s for s in by_sym if s["net"] < 0][:5],
        "cum_series": cum_series[-300:],  # cap to last 300 points
    }


HTML = r"""<!doctype html>
<html lang="ko"><head>
<meta charset="utf-8">
<title>Flipster Latency-Pick Live PnL</title>
<script src="https://cdn.jsdelivr.net/npm/chart.js@4.4.0/dist/chart.umd.min.js"></script>
<style>
body{font-family:ui-monospace,Menlo,Consolas,monospace;background:#0b0f14;color:#c9d1d9;margin:0;padding:16px;}
h1{font-size:18px;margin:0 0 8px;color:#7dd3fc;}
.sub{font-size:12px;color:#8b949e;margin-bottom:16px;}
.grid{display:grid;grid-template-columns:repeat(4,1fr);gap:12px;margin-bottom:16px;}
.card{background:#111826;border:1px solid #1f2937;border-radius:6px;padding:10px;}
.card h3{margin:0 0 4px;font-size:11px;color:#8b949e;text-transform:uppercase;font-weight:600;}
.card .v{font-size:22px;font-weight:700;}
.pos{color:#34d399;}.neg{color:#f87171;}.neutral{color:#e5e7eb;}
table{width:100%;border-collapse:collapse;font-size:13px;margin-bottom:16px;}
th,td{padding:6px 8px;text-align:right;border-bottom:1px solid #1f2937;}
th{color:#8b949e;font-weight:600;text-align:right;}
th:first-child,td:first-child{text-align:left;}
tr.tot td{border-top:2px solid #374151;font-weight:700;}
.flex{display:flex;gap:20px;margin-bottom:16px;}
.flex > div{flex:1;}
.chart{background:#111826;border:1px solid #1f2937;border-radius:6px;padding:12px;margin-bottom:16px;}
ul{list-style:none;padding:0;margin:0;font-size:12px;}
li{padding:2px 0;}
.err{color:#f87171;padding:20px;}
</style>
</head><body>
<h1>FLIPSTER LATENCY-PICK — LIVE PnL</h1>
<div class="sub" id="meta"></div>
<div id="content"></div>
<div class="chart"><canvas id="cumChart" height="100"></canvas></div>
<script>
const fmt = (x) => {
  const s = x >= 0 ? "+" : "";
  return s + "$" + x.toFixed(4);
};
const cls = (x) => x > 0 ? "pos" : (x < 0 ? "neg" : "neutral");
let chart = null;

async function refresh() {
  const r = await fetch("/api/stats");
  const d = await r.json();
  if (d.error) { document.getElementById("content").innerHTML = `<div class="err">${d.error}</div>`; return; }
  const st = d.status || {}, t = d.totals, ex = d.extras;
  document.getElementById("meta").textContent =
    `${d.now}  |  session ${(st.t_s/60).toFixed(1)} min  |  amount $${d.amount_usd}  |  fees maker ${d.fees.maker_bp}bp / taker ${d.fees.taker_bp}bp`;

  let html = `
  <div class="grid">
    <div class="card"><h3>True Net (settled)</h3><div class="v ${cls(t.net_settled)}">${fmt(t.net_settled)}</div></div>
    <div class="card"><h3>Balance Now (pre-rebate)</h3><div class="v ${cls(t.balance_view)}">${fmt(t.balance_view)}</div></div>
    <div class="card"><h3>Pending Rebate</h3><div class="v pos">+$${t.pending_rebate.toFixed(4)}</div></div>
    <div class="card"><h3>Gross (price only)</h3><div class="v ${cls(t.gross)}">${fmt(t.gross)}</div></div>
  </div>
  <div class="grid">
    <div class="card"><h3>Closes</h3><div class="v">${t.n}</div></div>
    <div class="card"><h3>WR</h3><div class="v">${t.wr}%</div></div>
    <div class="card"><h3>Open Pos / Tracker</h3><div class="v">${st.open_pos||0} / ${st.tracker||0}</div></div>
    <div class="card"><h3>Cancels (converged / timeout)</h3><div class="v" style="font-size:16px">${ex.cancel_converged} / ${ex.cancel_timeout} &nbsp;<small>LF ${ex.late_fills}</small></div></div>
  </div>
  <table>
    <tr><th>Reason</th><th>n</th><th>WR</th><th>avg move</th><th>Gross</th><th>Fees</th><th>Net</th></tr>`;
  for (const r of d.reasons) {
    html += `<tr><td>${r.reason}</td><td>${r.n}</td><td>${r.wr}%</td>
      <td class="${cls(r.avg_move_bp)}">${r.avg_move_bp>=0?"+":""}${r.avg_move_bp}bp</td>
      <td class="${cls(r.gross)}">${fmt(r.gross)}</td>
      <td class="neg">-$${r.fees.toFixed(4)}</td>
      <td class="${cls(r.net)}">${fmt(r.net)}</td></tr>`;
  }
  html += `<tr class="tot"><td>TOTAL</td><td>${t.n}</td><td>${t.wr}%</td><td></td>
    <td class="${cls(t.gross)}">${fmt(t.gross)}</td>
    <td class="neg">-$${t.fees_net.toFixed(4)}</td>
    <td class="${cls(t.net_settled)}">${fmt(t.net_settled)}</td></tr></table>
  <div class="flex">
    <div><strong>Winners (net):</strong><ul>
      ${d.winners.length ? d.winners.map(s => `<li class="pos">+${s.sym} &nbsp; n=${s.n} &nbsp; ${fmt(s.net)}</li>`).join("") : "<li>(none)</li>"}
    </ul></div>
    <div><strong>Losers (net):</strong><ul>
      ${d.losers.length ? d.losers.map(s => `<li class="neg">-${s.sym} &nbsp; n=${s.n} &nbsp; ${fmt(s.net)}</li>`).join("") : "<li>(none)</li>"}
    </ul></div>
  </div>`;
  document.getElementById("content").innerHTML = html;

  // Chart
  const labels = d.cum_series.map(p => p.i);
  const data = d.cum_series.map(p => p.net);
  const ctx = document.getElementById("cumChart").getContext("2d");
  if (!chart) {
    chart = new Chart(ctx, {type:"line", data:{labels,datasets:[{
      label:"Cumulative Net PnL ($)", data, borderColor:"#7dd3fc",
      backgroundColor:"rgba(125,211,252,.1)", fill:true, tension:.2, pointRadius:0}]},
      options:{animation:false, responsive:true, maintainAspectRatio:false,
        plugins:{legend:{labels:{color:"#c9d1d9"}}},
        scales:{x:{ticks:{color:"#8b949e"},grid:{color:"#1f2937"}},
                y:{ticks:{color:"#8b949e"},grid:{color:"#1f2937"}}}}});
  } else {
    chart.data.labels = labels;
    chart.data.datasets[0].data = data;
    chart.update("none");
  }
}
refresh();
setInterval(refresh, 5000);
</script>
</body></html>
"""


class Handler(BaseHTTPRequestHandler):
    log_path: Path = DEFAULT_LOG

    def do_GET(self):
        if self.path == "/":
            self.send_response(200)
            self.send_header("Content-Type", "text/html; charset=utf-8")
            self.end_headers()
            self.wfile.write(HTML.encode("utf-8"))
            return
        if self.path.startswith("/api/stats"):
            data = parse_log(self.log_path)
            body = json.dumps(data).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Cache-Control", "no-store")
            self.end_headers()
            self.wfile.write(body)
            return
        self.send_response(404); self.end_headers()

    def log_message(self, *a, **kw):
        pass  # silence default access log


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=8080)
    ap.add_argument("--log", type=str, default=str(DEFAULT_LOG))
    args = ap.parse_args()
    Handler.log_path = Path(args.log)
    srv = ThreadingHTTPServer(("0.0.0.0", args.port), Handler)
    print(f"[dashboard] http://0.0.0.0:{args.port}/  (log={args.log})", flush=True)
    try:
        srv.serve_forever()
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
