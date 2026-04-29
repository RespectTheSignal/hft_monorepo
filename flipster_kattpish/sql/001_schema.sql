CREATE TABLE IF NOT EXISTS binance_bookticker (
  symbol SYMBOL CAPACITY 500 CACHE,
  bid_price DOUBLE, ask_price DOUBLE,
  last_price DOUBLE, mark_price DOUBLE, index_price DOUBLE,
  bid_size DOUBLE, ask_size DOUBLE,
  timestamp TIMESTAMP
) TIMESTAMP(timestamp) PARTITION BY DAY WAL;

CREATE TABLE IF NOT EXISTS flipster_bookticker (
  symbol SYMBOL CAPACITY 500 CACHE,
  bid_price DOUBLE, ask_price DOUBLE,
  last_price DOUBLE, mark_price DOUBLE, index_price DOUBLE,
  bid_size DOUBLE, ask_size DOUBLE,
  timestamp TIMESTAMP
) TIMESTAMP(timestamp) PARTITION BY DAY WAL;

CREATE TABLE IF NOT EXISTS bybit_bookticker (
  symbol SYMBOL CAPACITY 500 CACHE,
  bid_price DOUBLE, ask_price DOUBLE,
  last_price DOUBLE, mark_price DOUBLE, index_price DOUBLE,
  bid_size DOUBLE, ask_size DOUBLE,
  timestamp TIMESTAMP
) TIMESTAMP(timestamp) PARTITION BY DAY WAL;

CREATE TABLE IF NOT EXISTS bitget_bookticker (
  symbol SYMBOL CAPACITY 500 CACHE,
  bid_price DOUBLE, ask_price DOUBLE,
  last_price DOUBLE, mark_price DOUBLE, index_price DOUBLE,
  bid_size DOUBLE, ask_size DOUBLE,
  timestamp TIMESTAMP
) TIMESTAMP(timestamp) PARTITION BY DAY WAL;

CREATE TABLE IF NOT EXISTS gate_bookticker (
  symbol SYMBOL CAPACITY 500 CACHE,
  bid_price DOUBLE, ask_price DOUBLE,
  last_price DOUBLE, mark_price DOUBLE, index_price DOUBLE,
  bid_size DOUBLE, ask_size DOUBLE,
  timestamp TIMESTAMP
) TIMESTAMP(timestamp) PARTITION BY DAY WAL;

CREATE TABLE IF NOT EXISTS latency_log (
  symbol SYMBOL CAPACITY 500 CACHE,
  binance_ts TIMESTAMP,
  flipster_ts TIMESTAMP,
  latency_ms DOUBLE,
  binance_mid DOUBLE,
  flipster_mid DOUBLE,
  price_diff_bp DOUBLE,
  direction INT
) TIMESTAMP(binance_ts) PARTITION BY DAY WAL;

CREATE TABLE IF NOT EXISTS funding_rate (
  exchange SYMBOL CAPACITY 16 CACHE,
  symbol SYMBOL CAPACITY 500 CACHE,
  rate DOUBLE,
  next_funding_time TIMESTAMP,
  timestamp TIMESTAMP
) TIMESTAMP(timestamp) PARTITION BY DAY WAL;

CREATE TABLE IF NOT EXISTS orderbook_snapshot (
  exchange SYMBOL CAPACITY 16 CACHE,
  symbol SYMBOL CAPACITY 500 CACHE,
  side SYMBOL CAPACITY 4 CACHE,
  price DOUBLE,
  size DOUBLE,
  timestamp TIMESTAMP
) TIMESTAMP(timestamp) PARTITION BY DAY WAL;

CREATE TABLE IF NOT EXISTS position_log (
  account_id SYMBOL CAPACITY 16 CACHE,
  symbol SYMBOL CAPACITY 500 CACHE,
  side SYMBOL CAPACITY 4 CACHE,
  entry_price DOUBLE,
  exit_price DOUBLE,
  size DOUBLE,
  pnl_bp DOUBLE,
  entry_time TIMESTAMP,
  exit_time TIMESTAMP,
  strategy SYMBOL CAPACITY 16 CACHE,
  exit_reason SYMBOL CAPACITY 16 CACHE,
  mode SYMBOL CAPACITY 4 CACHE
) TIMESTAMP(entry_time) PARTITION BY DAY WAL;

CREATE TABLE IF NOT EXISTS collector_health (
  exchange SYMBOL CAPACITY 16 CACHE,
  event SYMBOL CAPACITY 16 CACHE,
  detail STRING,
  timestamp TIMESTAMP
) TIMESTAMP(timestamp) PARTITION BY DAY WAL;
