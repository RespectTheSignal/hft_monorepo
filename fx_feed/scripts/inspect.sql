-- fx_feed QuestDB 분석 쿼리
-- QuestDB 웹 콘솔: http://211.181.122.102:9000

-- 1. 최근 5분간 소스별 수신 count + wire delay 분포
--    wire_delay_ms = recv_ns - source_ns (거래소 → 우리)
SELECT
    source,
    count() AS ticks,
    avg(wire_delay_ms)  AS avg_wire_ms,
    percentile(wire_delay_ms, 50) AS p50_wire_ms,
    percentile(wire_delay_ms, 99) AS p99_wire_ms,
    max(wire_delay_ms)  AS max_wire_ms,
    avg(total_delay_ms) AS avg_total_ms,
    percentile(total_delay_ms, 99) AS p99_total_ms
FROM fx_bookticker
WHERE timestamp > dateadd('m', -5, now())
GROUP BY source
ORDER BY avg_wire_ms;

-- 1-b. Delay breakdown (어느 stage가 병목인가)
SELECT
    source,
    count() AS ticks,
    avg(wire_delay_ms)  AS wire,     -- 거래소 → WS recv
    avg(parse_delay_ms) AS parse,    -- recv → JSON parse
    avg(queue_delay_ms) AS queue,    -- parse → ILP buffer
    avg(local_delay_ms) AS local,    -- recv → buffer (parse + queue)
    avg(exchange_delay_ms) AS exch   -- 거래소 내부 (Binance E-T, Databento ts_recv-ts_event)
FROM fx_bookticker
WHERE timestamp > dateadd('m', -5, now())
GROUP BY source;

-- 2. 심볼 × 소스 매트릭스 — 어느 소스가 어떤 심볼을 커버하는가
SELECT
    symbol,
    source,
    count() AS ticks,
    percentile(wire_delay_ms, 50) AS p50_wire_ms
FROM fx_bookticker
WHERE timestamp > dateadd('m', -5, now())
GROUP BY symbol, source
ORDER BY symbol, p50_wire_ms;

-- 3. 최근 1분 spread_bps (eyeball용)
SELECT
    source,
    symbol,
    avg((ask_price - bid_price) / ((bid_price + ask_price) / 2) * 10000) AS spread_bps,
    min((ask_price - bid_price) / ((bid_price + ask_price) / 2) * 10000) AS min_bps,
    max((ask_price - bid_price) / ((bid_price + ask_price) / 2) * 10000) AS max_bps,
    count() AS ticks
FROM fx_bookticker
WHERE timestamp > dateadd('m', -1, now())
    AND bid_price > 0 AND ask_price > 0
GROUP BY source, symbol
ORDER BY symbol, spread_bps;

-- 4. 소스 간 cross-source latency gap — 같은 심볼을 누가 더 빨리 보냈나
-- 1초 버킷에서 소스별 mid 비교
SELECT
    timestamp,
    symbol,
    last(bid_price) FILTER (WHERE source = 'binance') AS binance_bid,
    last(bid_price) FILTER (WHERE source = 'dxfeed') AS dxfeed_bid,
    last(bid_price) FILTER (WHERE source = 'databento') AS databento_bid
FROM fx_bookticker
WHERE timestamp > dateadd('m', -5, now())
SAMPLE BY 1s ALIGN TO CALENDAR
LIMIT 100;

-- 5. 체감 tick rate (초당 틱수) — 소스별
SELECT
    source,
    symbol,
    count() / 60.0 AS ticks_per_sec
FROM fx_bookticker
WHERE timestamp > dateadd('m', -1, now())
GROUP BY source, symbol
ORDER BY ticks_per_sec DESC;
