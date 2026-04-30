#!/usr/bin/env python3
"""Debugging dashboard for gate_lead trades.

Shows Binance + Flipster price overlaid on a single chart with entry/exit
markers from position_log, so you can see exactly what the market was
doing at every signal. Built for "why did we enter here?" debugging.

Backend: tiny HTTP server, queries central QuestDB directly.
Frontend: single HTML page, vanilla JS + Plotly for the chart.

Run:
  scripts/debug_dashboard.py [--port 8090]
Open: http://gate1:8090
"""
import argparse
import http.server
import json
import socketserver
import urllib.parse
import urllib.request

QDB = "http://211.181.122.102:9000"


def qdb(sql: str) -> dict:
    url = f"{QDB}/exec?query={urllib.parse.quote(sql)}"
    with urllib.request.urlopen(url, timeout=15) as r:
        return json.load(r)


def to_dicts(resp: dict) -> list[dict]:
    cols = [c["name"] for c in resp.get("columns", [])]
    return [dict(zip(cols, row)) for row in resp.get("dataset", [])]


HTML = r"""<!doctype html>
<meta charset=utf-8>
<title>gate_lead debug dashboard</title>
<script src="https://cdn.plot.ly/plotly-2.35.2.min.js"></script>
<style>
  body { font: 13px/1.4 system-ui, sans-serif; margin: 0; background: #0e0f12; color: #ddd; }
  .top { padding: 8px 12px; background: #1a1c20; border-bottom: 1px solid #333; display: flex; gap: 12px; align-items: center; flex-wrap: wrap; }
  .top label { color: #999; font-size: 12px; }
  .top input, .top select, .top button { background: #2a2d33; color: #ddd; border: 1px solid #444; padding: 4px 8px; font: inherit; border-radius: 3px; }
  .top button { cursor: pointer; }
  .top button:hover { background: #3a3d43; }
  .top button.on { background: #2c5; color: #000; }
  .main { display: grid; grid-template-columns: 1fr 380px; height: calc(100vh - 50px); }
  #chart { background: #0e0f12; }
  .side { background: #14161b; border-left: 1px solid #2a2d33; overflow-y: auto; padding: 8px; }
  .side h3 { margin: 4px 0 8px; color: #888; font-size: 11px; text-transform: uppercase; letter-spacing: .5px; }
  .trade { padding: 6px 8px; border-radius: 3px; margin-bottom: 4px; cursor: pointer; font-family: ui-monospace, monospace; font-size: 12px; }
  .trade.win { background: rgba(40, 200, 100, 0.10); border-left: 3px solid #2c5; }
  .trade.lose { background: rgba(220, 60, 60, 0.10); border-left: 3px solid #c33; }
  .trade:hover { background: rgba(255,255,255,0.06); }
  .trade.sym-cur { outline: 1px solid #58a6ff; }
  .trade .h { display: flex; justify-content: space-between; }
  .trade .sym { font-weight: 600; }
  .trade .pnl { font-variant-numeric: tabular-nums; }
  .trade .meta { color: #888; font-size: 11px; margin-top: 2px; }
  .stat { font-family: ui-monospace, monospace; font-size: 12px; color: #aaa; }
  .err { color: #c33; padding: 8px; }
</style>

<div class="top">
  <label>symbol</label><select id=sym></select>
  <label>window</label>
  <button data-min="1">1m</button>
  <button data-min="2">2m</button>
  <button data-min="5" class=cur>5m</button>
  <button data-min="15">15m</button>
  <button data-min="60">1h</button>
  <label>end</label><input type=text id=end placeholder="now" size=20>
  <button id=back>« back</button>
  <button id=fwd>fwd »</button>
  <button id=now>now</button>
  <span style="flex:1"></span>
  <label>refresh</label><select id=refresh>
    <option value=0>off</option>
    <option value=2 selected>2s</option>
    <option value=5>5s</option>
    <option value=10>10s</option>
    <option value=30>30s</option>
  </select>
  <button id=reload>↻</button>
</div>

<div class="main">
  <div id=chart></div>
  <div class="side">
    <h3>summary</h3>
    <div id=stats class=stat>—</div>
    <h3 style="margin-top:14px">recent trades — all symbols (click to jump)</h3>
    <div id=trades>—</div>
  </div>
</div>

<script>
let CUR_MIN = 5
let CUR_END = null    // null = follow now
let SYM = null
let LIVE_TIMER = null

const $ = s => document.querySelector(s)

async function fetchJSON(url) {
  const r = await fetch(url)
  if (!r.ok) throw new Error(r.status + ' ' + r.statusText)
  return await r.json()
}

async function loadSymbols() {
  const list = await fetchJSON('/api/symbols')
  const sel = $('#sym')
  sel.innerHTML = ''
  for (const s of list) {
    const opt = document.createElement('option')
    opt.value = s; opt.textContent = s
    sel.appendChild(opt)
  }
  SYM = list[0] || ''
  // Try restoring from URL hash
  if (location.hash.length > 1) {
    const h = decodeURIComponent(location.hash.slice(1))
    if (list.includes(h)) SYM = h
  }
  sel.value = SYM
}

function rangeIso() {
  const end = CUR_END ? new Date(CUR_END) : new Date()
  const start = new Date(end.getTime() - CUR_MIN * 60 * 1000)
  return [start.toISOString(), end.toISOString()]
}

async function reload() {
  if (!SYM) return
  location.hash = SYM
  const [s, e] = rangeIso()
  // Fetch chart data for current symbol + recent trades across ALL symbols
  let data, recent
  try {
    [data, recent] = await Promise.all([
      fetchJSON(`/api/data?symbol=${SYM}&start=${s}&end=${e}`),
      fetchJSON(`/api/recent?n=80`),
    ])
  } catch (err) {
    $('#chart').innerHTML = `<div class=err>${err}</div>`
    return
  }
  drawChart(data)
  drawStats(data, recent)
  drawTrades(recent)
}

function drawChart(d) {
  const traces = []

  // Compute spread (Flipster - Binance) in bp by interpolating to a
  // common time grid (every 200ms). Both feeds tick at different
  // rates, so plain subtraction needs same x-axis. We use the union
  // of timestamps and step-fill the most recent value.
  const spread = computeSpread(d.binance, d.flipster)

  // Top panel: Binance + Flipster prices (STEP — horizontal+vertical only,
  // tick value held until next update; matches how the orderbook actually
  // behaves and avoids misleading diagonal interpolation).
  if (d.binance && d.binance.t.length) {
    traces.push({
      x: d.binance.t, y: d.binance.mid,
      type: 'scattergl', mode: 'lines', name: 'Binance',
      line: { color: '#ff5499', width: 2, shape: 'hv' },
      xaxis: 'x', yaxis: 'y',
    })
  }
  if (d.flipster && d.flipster.t.length) {
    traces.push({
      x: d.flipster.t, y: d.flipster.mid,
      type: 'scattergl', mode: 'lines', name: 'Flipster',
      line: { color: '#54aaff', width: 2, shape: 'hv' },
      xaxis: 'x', yaxis: 'y',
    })
  }

  // Hold-period bars: connect entry → exit at entry price (green=win,red=loss)
  for (const t of d.trades) {
    traces.push({
      x: [t.entry_ts, t.exit_ts],
      y: [t.entry_price, t.entry_price],
      type: 'scattergl', mode: 'lines', name: '',
      line: { color: t.pnl_bp > 0 ? 'rgba(40,200,100,0.55)' : 'rgba(220,60,60,0.55)', width: 8 },
      showlegend: false, hoverinfo: 'skip',
      xaxis: 'x', yaxis: 'y',
    })
  }

  // Entry markers — large solid triangle, white outline, drop-shadow ring
  if (d.trades.length) {
    // Outer halo (bigger, semi-transparent) — makes the marker visible against
    // dense price lines without obscuring them.
    traces.push({
      x: d.trades.map(t => t.entry_ts),
      y: d.trades.map(t => t.entry_price),
      type: 'scattergl', mode: 'markers',
      marker: {
        symbol: d.trades.map(t => t.side === 'long' ? 'triangle-up' : 'triangle-down'),
        size: 28,
        color: d.trades.map(t => t.side === 'long' ? 'rgba(40,200,100,0.25)' : 'rgba(220,60,60,0.25)'),
        line: { width: 0 },
      },
      showlegend: false, hoverinfo: 'skip',
      xaxis: 'x', yaxis: 'y',
    })
    traces.push({
      x: d.trades.map(t => t.entry_ts),
      y: d.trades.map(t => t.entry_price),
      text: d.trades.map(t => `${t.side.toUpperCase()} entry @ ${t.entry_price}`),
      type: 'scattergl', mode: 'markers', name: 'Entry',
      marker: {
        symbol: d.trades.map(t => t.side === 'long' ? 'triangle-up' : 'triangle-down'),
        size: 18,
        color: d.trades.map(t => t.side === 'long' ? '#2c5' : '#c33'),
        line: { width: 2, color: '#fff' },
      },
      hovertemplate: '%{text}<br>%{x}<extra></extra>',
      xaxis: 'x', yaxis: 'y',
    })
    // Exit markers — diamond colored by W/L, white border
    traces.push({
      x: d.trades.map(t => t.exit_ts),
      y: d.trades.map(t => t.exit_price),
      type: 'scattergl', mode: 'markers',
      marker: {
        symbol: 'diamond',
        size: 26,
        color: d.trades.map(t => t.pnl_bp > 0 ? 'rgba(40,200,100,0.20)' : 'rgba(220,60,60,0.20)'),
        line: { width: 0 },
      },
      showlegend: false, hoverinfo: 'skip',
      xaxis: 'x', yaxis: 'y',
    })
    traces.push({
      x: d.trades.map(t => t.exit_ts),
      y: d.trades.map(t => t.exit_price),
      text: d.trades.map(t => `exit ${t.exit_reason} ${t.pnl_bp.toFixed(2)}bp`),
      type: 'scattergl', mode: 'markers', name: 'Exit',
      marker: {
        symbol: 'diamond',
        size: 14,
        color: d.trades.map(t => t.pnl_bp > 0 ? '#2c5' : '#c33'),
        line: { width: 2, color: '#fff' },
      },
      hovertemplate: '%{text}<br>%{x}<extra></extra>',
      xaxis: 'x', yaxis: 'y',
    })
  }

  // Bottom panel: spread (Flipster - Binance) in bp — also stepped
  if (spread.t.length) {
    traces.push({
      x: spread.t, y: spread.bp,
      type: 'scattergl', mode: 'lines', name: 'lag (Flipster - Binance, bp)',
      line: { color: '#ffcc55', width: 1.5, shape: 'hv' },
      fill: 'tozeroy', fillcolor: 'rgba(255,204,85,0.10)',
      xaxis: 'x', yaxis: 'y2',
    })
  }

  const layout = {
    margin: { t: 10, b: 30, l: 60, r: 10 },
    paper_bgcolor: '#0e0f12', plot_bgcolor: '#0e0f12',
    font: { color: '#ddd', size: 11 },
    xaxis: { gridcolor: '#222', zerolinecolor: '#333', showspikes: true,
             spikemode: 'across', spikethickness: 1, spikedash: 'dot',
             domain: [0, 1], anchor: 'y2' },
    yaxis: { gridcolor: '#222', zerolinecolor: '#333',
             domain: [0.32, 1], title: { text: 'price', font: { size: 10 } } },
    yaxis2: { gridcolor: '#222', zerolinecolor: '#555',
              domain: [0, 0.28], title: { text: 'lag bp', font: { size: 10 } } },
    legend: { x: 0, y: 1, bgcolor: 'rgba(0,0,0,0.4)', font: { size: 10 } },
    hovermode: 'x unified',
    shapes: buildShapes(d),
  }
  Plotly.react('chart', traces, layout, { displayModeBar: false, responsive: true })
}

// Shapes: zero-line on lag panel + a vertical guide at every trade
// entry/exit, spanning both subplots (yref: paper, 0..1). Entry guides
// are tinted by side (green long, red short), exit guides by W/L.
function buildShapes(d) {
  const shapes = [
    { type: 'line', xref: 'paper', x0: 0, x1: 1, yref: 'y2',
      y0: 0, y1: 0, line: { color: '#444', width: 1 } },
  ]
  for (const t of d.trades || []) {
    shapes.push({
      type: 'line', xref: 'x', yref: 'paper',
      x0: t.entry_ts, x1: t.entry_ts, y0: 0, y1: 1,
      line: {
        color: t.side === 'long' ? 'rgba(40,200,100,0.45)' : 'rgba(220,60,60,0.45)',
        width: 1.5, dash: 'dot',
      },
      layer: 'below',
    })
    shapes.push({
      type: 'line', xref: 'x', yref: 'paper',
      x0: t.exit_ts, x1: t.exit_ts, y0: 0, y1: 1,
      line: {
        color: t.pnl_bp > 0 ? 'rgba(40,200,100,0.35)' : 'rgba(220,60,60,0.35)',
        width: 1, dash: 'dash',
      },
      layer: 'below',
    })
  }
  return shapes
}

function computeSpread(b, f) {
  if (!b || !f || !b.t.length || !f.t.length) return { t: [], bp: [] }
  // Step-aligned: at each Binance tick, find latest Flipster tick before it
  const out = { t: [], bp: [] }
  let fi = 0
  for (let i = 0; i < b.t.length; i++) {
    while (fi + 1 < f.t.length && f.t[fi + 1] <= b.t[i]) fi++
    if (f.t[fi] > b.t[i]) continue  // no Flipster tick before this Binance tick yet
    const bm = b.mid[i], fm = f.mid[fi]
    if (bm > 0 && fm > 0) {
      out.t.push(b.t[i])
      out.bp.push((fm - bm) / bm * 1e4)
    }
  }
  return out
}

function drawStats(d, recent) {
  const tr = d.trades || []
  const symW = tr.filter(t => t.pnl_bp > 0).length
  const symL = tr.filter(t => t.pnl_bp <= 0).length
  const symBp = tr.reduce((a,t) => a + t.pnl_bp, 0)
  const symUsd = tr.reduce((a,t) => a + t.pnl_bp * t.size / 1e4, 0)
  // Global recent stats (ALL symbols)
  const all = recent || []
  const allW = all.filter(t => t.pnl_bp > 0).length
  const allUsd = all.reduce((a,t) => a + t.pnl_bp * t.size / 1e4, 0)
  $('#stats').innerHTML =
    `<b>${SYM}</b> in window: n=${tr.length} W/L=${symW}/${symL} ${tr.length ? (symW/tr.length*100).toFixed(0)+'%' : '-'}<br>` +
    `   Σ ${symBp.toFixed(1)}bp / $${symUsd.toFixed(2)}<br>` +
    `<br><b>last ${all.length} (all syms)</b>: ${allW}W ${all.length-allW}L ${all.length ? (allW/all.length*100).toFixed(0)+'%' : '-'}<br>` +
    `   Σ $${allUsd.toFixed(2)}`
}

function drawTrades(tr) {
  const cont = $('#trades')
  if (!tr || !tr.length) { cont.innerHTML = '<div class=stat>(no recent trades)</div>'; return }
  const sorted = [...tr].sort((a,b) => b.entry_ts.localeCompare(a.entry_ts))
  cont.innerHTML = ''
  for (const t of sorted) {
    const div = document.createElement('div')
    div.className = 'trade ' + (t.pnl_bp > 0 ? 'win' : 'lose')
    if (t.symbol === SYM) div.classList.add('sym-cur')
    const sideArrow = t.side === 'long' ? '▲ L' : '▼ S'
    div.innerHTML = `
      <div class=h><span class=sym>${sideArrow} ${t.symbol}</span><span class=pnl>${t.pnl_bp >= 0 ? '+' : ''}${t.pnl_bp.toFixed(1)}bp</span></div>
      <div class=meta>${t.entry_ts.slice(11,19)} → ${t.exit_ts.slice(11,19)} · ${t.exit_reason} · $${t.size.toFixed(0)}</div>`
    div.onclick = () => {
      // Switch to this symbol + center window on the trade
      SYM = t.symbol
      $('#sym').value = t.symbol
      const c = new Date(t.entry_ts).getTime()
      const halfMs = CUR_MIN * 60 * 1000 / 2
      CUR_END = new Date(c + halfMs).toISOString()
      $('#end').value = CUR_END
      reload()
    }
    cont.appendChild(div)
  }
}

function setupRefresh() {
  if (LIVE_TIMER) { clearInterval(LIVE_TIMER); LIVE_TIMER = null }
  const sec = parseInt($('#refresh').value)
  if (sec > 0) {
    LIVE_TIMER = setInterval(reload, sec * 1000)
  }
}

document.querySelectorAll('button[data-min]').forEach(b => {
  b.onclick = () => {
    document.querySelectorAll('button[data-min]').forEach(x => x.classList.remove('cur'))
    b.classList.add('cur')
    CUR_MIN = parseInt(b.dataset.min)
    reload()
  }
})
$('#sym').onchange = e => { SYM = e.target.value; reload() }
$('#end').onchange = e => { CUR_END = e.target.value || null; reload() }
$('#back').onclick = () => {
  const end = CUR_END ? new Date(CUR_END) : new Date()
  CUR_END = new Date(end.getTime() - CUR_MIN * 60 * 1000).toISOString()
  $('#end').value = CUR_END; reload()
}
$('#fwd').onclick = () => {
  if (!CUR_END) return
  const end = new Date(CUR_END)
  CUR_END = new Date(end.getTime() + CUR_MIN * 60 * 1000).toISOString()
  $('#end').value = CUR_END; reload()
}
$('#now').onclick = () => { CUR_END = null; $('#end').value = ''; reload() }
$('#refresh').onchange = setupRefresh
$('#reload').onclick = reload

;(async () => {
  await loadSymbols()
  reload()
  setupRefresh()
})()
</script>
"""


class Handler(http.server.SimpleHTTPRequestHandler):
    def log_message(self, format, *args):
        pass  # quiet

    def do_GET(self):
        try:
            self._do_GET()
        except Exception as e:
            self.send_response(500)
            self.send_header("Content-Type", "text/plain")
            self.end_headers()
            self.wfile.write(str(e).encode())

    def _do_GET(self):
        u = urllib.parse.urlparse(self.path)
        if u.path == "/" or u.path == "/index.html":
            body = HTML.encode()
            self.send_response(200)
            self.send_header("Content-Type", "text/html; charset=utf-8")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        if u.path == "/api/symbols":
            sql = """
                SELECT DISTINCT symbol FROM position_log
                WHERE strategy='gate_lead' AND mode='paper'
                  AND timestamp > dateadd('h', -48, now())
                ORDER BY symbol
            """
            r = qdb(sql)
            syms = sorted({row[0] for row in r.get("dataset", [])})
            self._json(syms)
            return
        if u.path == "/api/recent":
            qs = urllib.parse.parse_qs(u.query)
            n = int(qs.get("n", ["80"])[0])
            sql = f"""
                SELECT timestamp entry_ts, exit_time exit_ts, side,
                       entry_price, exit_price, size, pnl_bp,
                       exit_reason, symbol
                FROM position_log
                WHERE strategy='gate_lead' AND mode='paper'
                  AND timestamp > dateadd('h', -2, now())
                ORDER BY timestamp DESC
                LIMIT {n}
            """
            self._json(to_dicts(qdb(sql)))
            return
        if u.path == "/api/data":
            qs = urllib.parse.parse_qs(u.query)
            sym = qs.get("symbol", [""])[0]
            start = qs.get("start", [""])[0]
            end = qs.get("end", [""])[0]
            if not (sym and start and end):
                self._json({"err": "missing args"})
                return
            data = self._fetch_data(sym, start, end)
            self._json(data)
            return
        self.send_response(404); self.end_headers()

    def _fetch_data(self, sym: str, start: str, end: str) -> dict:
        # binance: BASE_USDT format
        bin_sql = f"""
            SELECT timestamp, (bid_price + ask_price) / 2 mid
            FROM binance_bookticker
            WHERE symbol='{sym}_USDT'
              AND timestamp >= '{start}' AND timestamp <= '{end}'
            ORDER BY timestamp
        """
        # flipster: BASEUSDT.PERP format
        flip_sql = f"""
            SELECT timestamp, (bid_price + ask_price) / 2 mid
            FROM flipster_bookticker
            WHERE symbol='{sym}USDT.PERP'
              AND timestamp >= '{start}' AND timestamp <= '{end}'
            ORDER BY timestamp
        """
        # trades from gate_lead live (BINANCE_LEAD_v1)
        trade_sql = f"""
            SELECT timestamp entry_ts, exit_time exit_ts, side,
                   entry_price, exit_price, size, pnl_bp,
                   exit_reason, symbol
            FROM position_log
            WHERE strategy='gate_lead' AND mode='paper'
              AND symbol='{sym}'
              AND timestamp >= '{start}' AND timestamp <= '{end}'
            ORDER BY timestamp DESC
            LIMIT 200
        """
        bin_r = qdb(bin_sql)
        flip_r = qdb(flip_sql)
        trd_r = qdb(trade_sql)

        bin_t, bin_m = [], []
        for row in bin_r.get("dataset", []):
            bin_t.append(row[0]); bin_m.append(row[1])
        flip_t, flip_m = [], []
        for row in flip_r.get("dataset", []):
            flip_t.append(row[0]); flip_m.append(row[1])

        return {
            "binance": {"t": bin_t, "mid": bin_m},
            "flipster": {"t": flip_t, "mid": flip_m},
            "trades": to_dicts(trd_r),
        }

    def _json(self, obj) -> None:
        body = json.dumps(obj).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


def main() -> None:
    p = argparse.ArgumentParser()
    p.add_argument("--port", type=int, default=8090)
    p.add_argument("--host", default="0.0.0.0")
    args = p.parse_args()

    socketserver.ThreadingTCPServer.allow_reuse_address = True
    with socketserver.ThreadingTCPServer((args.host, args.port), Handler) as srv:
        print(f"dashboard → http://{args.host}:{args.port}")
        srv.serve_forever()


if __name__ == "__main__":
    main()
