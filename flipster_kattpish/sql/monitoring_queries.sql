-- =====================================================================
-- Monitoring query cheatsheet — paste into http://127.0.0.1:9000
-- Each block is independent. Works against live data in QuestDB.
-- =====================================================================

-- [A] System health: ticks per minute per exchange (last 15m)
SELECT timestamp, count() n
FROM binance_bookticker
WHERE timestamp > dateadd('m', -15, now())
SAMPLE BY 1m ALIGN TO CALENDAR;

-- [B] Data freshness per exchange (last tick age, rows last 10m)
SELECT 'binance' ex, count() rows_10m, max(timestamp) last_ts FROM binance_bookticker WHERE timestamp > dateadd('m', -10, now())
UNION ALL
SELECT 'bybit',   count(),     max(timestamp) FROM bybit_bookticker   WHERE timestamp > dateadd('m', -10, now())
UNION ALL
SELECT 'bitget',  count(),     max(timestamp) FROM bitget_bookticker  WHERE timestamp > dateadd('m', -10, now())
UNION ALL
SELECT 'gate',    count(),     max(timestamp) FROM gate_bookticker    WHERE timestamp > dateadd('m', -10, now())
UNION ALL
SELECT 'flipster',count(),     max(timestamp) FROM flipster_bookticker WHERE timestamp > dateadd('m', -10, now());

-- [C] BTC mid overlay — all exchanges aligned at 1s buckets (last 10m)
WITH
  b AS (SELECT timestamp time, avg((bid_price+ask_price)/2) m FROM binance_bookticker WHERE symbol='BTCUSDT' AND timestamp>dateadd('m',-10,now()) SAMPLE BY 1s ALIGN TO CALENDAR),
  y AS (SELECT timestamp time, avg((bid_price+ask_price)/2) m FROM bybit_bookticker   WHERE symbol='BTCUSDT' AND timestamp>dateadd('m',-10,now()) SAMPLE BY 1s ALIGN TO CALENDAR),
  g AS (SELECT timestamp time, avg((bid_price+ask_price)/2) m FROM gate_bookticker    WHERE symbol='BTCUSDT' AND timestamp>dateadd('m',-10,now()) SAMPLE BY 1s ALIGN TO CALENDAR)
SELECT b.time, b.m binance, y.m bybit, g.m gate
FROM b
ASOF JOIN y ON (time)
ASOF JOIN g ON (time);

-- [D] Spread bp — gate vs binance for BTC (last 30m). Positive = gate richer.
WITH
  b AS (SELECT timestamp time, avg((bid_price+ask_price)/2) m FROM binance_bookticker WHERE symbol='BTCUSDT' AND timestamp>dateadd('m',-30,now()) SAMPLE BY 1s ALIGN TO CALENDAR),
  g AS (SELECT timestamp time, avg((bid_price+ask_price)/2) m FROM gate_bookticker    WHERE symbol='BTCUSDT' AND timestamp>dateadd('m',-30,now()) SAMPLE BY 1s ALIGN TO CALENDAR)
SELECT b.time, (g.m - b.m) / b.m * 10000 gate_minus_binance_bp
FROM b ASOF JOIN g;

-- [E] Latency ranking per symbol (24h) — fills in once flipster feed is active
SELECT symbol, avg(latency_ms) avg_ms, count() n
FROM latency_log
WHERE binance_ts > dateadd('d', -1, now())
GROUP BY symbol
ORDER BY avg_ms DESC;

-- [F] Signal rate (latency events per hour, last 6h)
SELECT binance_ts time, count() signals
FROM latency_log
WHERE binance_ts > dateadd('h', -6, now())
SAMPLE BY 1h ALIGN TO CALENDAR;

-- [G] Funding rate snapshot — sorted by most negative (short-hedge candidates)
SELECT exchange, symbol, last(rate) rate, last(next_funding_time) next_funding
FROM funding_rate
WHERE timestamp > dateadd('h', -2, now())
GROUP BY exchange, symbol
ORDER BY rate ASC
LIMIT 30;

-- [H] Funding rate history (last 7d, top 6 symbols)
SELECT timestamp, symbol, rate
FROM funding_rate
WHERE exchange='binance'
  AND symbol IN ('BTCUSDT','ETHUSDT','SOLUSDT','XRPUSDT','BNBUSDT','DOGEUSDT')
  AND timestamp > dateadd('d', -7, now())
ORDER BY timestamp;

-- [I] Paper-bot cumulative PnL (24h)
SELECT entry_time time,
       sum(pnl_bp) OVER (ORDER BY entry_time) cum_pnl_bp
FROM position_log
WHERE mode='paper' AND entry_time > dateadd('d', -1, now())
ORDER BY entry_time;

-- [J] Per-symbol PnL breakdown (paper)
SELECT symbol,
       count()          trades,
       avg(pnl_bp)      avg_bp,
       sum(pnl_bp)      total_bp,
       sum(CASE WHEN pnl_bp > 0 THEN 1.0 ELSE 0.0 END) / count() hit_rate
FROM position_log
WHERE mode='paper' AND entry_time > dateadd('d', -1, now())
GROUP BY symbol
ORDER BY total_bp DESC;

-- [K] Zero-spread scan on flipster (requires flipster feed)
SELECT symbol,
       last(bid_price)  bid,
       last(ask_price)  ask,
       (last(ask_price) - last(bid_price)) / last(bid_price) * 10000 spread_bp
FROM flipster_bookticker
WHERE timestamp > dateadd('m', -5, now())
GROUP BY symbol
ORDER BY spread_bp ASC;

-- [L] Collector health events — recent connect/disconnect
SELECT timestamp, exchange, event, detail
FROM collector_health
WHERE timestamp > dateadd('h', -1, now())
ORDER BY timestamp DESC
LIMIT 200;
