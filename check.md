# HFT Rewrite — Legacy Feature Completeness Checklist

> **목적**: 기존 `gate_hft/` 레포의 모든 기능을 `hft_monorepo/mono/` 로 포팅하는 과정에서 빠뜨림 없이 검증하기 위한 체크리스트.
>
> **사용법**: 구현이 완료되면 `- [ ]` → `- [x]` 로 체크. 각 항목 우측에 **→ target crate** 를 명시.
> 기능이 의도적으로 drop 되면 `- [~]` 로 표시하고 **Drop reason** 라인 추가.
>
> **Scope 범례**:
> - ✅ **P1 (Phase 1)** — 최소 MVP. 기능 누락 시 v1 릴리즈 불가.
> - 🔶 **P2 (Phase 2)** — strategy / risk. v1 이후 추가.
> - 🔷 **P3 (Phase 3)** — ops / analytics. 별도 트랙.
> - ⏸ **Deferred / out-of-scope** — Python 전용 운영 스크립트 등. 포팅 안 함.

---

## 0. 전역 상수 / 매직 넘버 (모든 crate 공통 참조)

- [ ] `IGNORE_SYMBOLS = ["BTC_USDT", "ETH_USDT", "OGN_USDT"]` — publisher worker 필터 → `hft-core-config`
- [ ] `EXCLUDED_SYMBOLS = ["BTC_USDT", "ETH_USDT"]` — strategy 필터 → `hft-core-config`
- [ ] `EPS = 1e-12` — 0-division guard → `hft-types::consts`
- [ ] `MAX_POSITION_SIDE_RATIO = 0.5` — 계좌 balance 대비 net exposure 상한 → `hft-strategy::risk`
- [ ] `DEFAULT_LEVERAGE = 50.0` — risk_manager 기본값 (handle_chance 에서는 1000.0) → `hft-strategy::risk`
- [ ] `ZMQ_SNDHWM = 100_000` / `ZMQ_RCVHWM = 100_000` → `hft-zmq::defaults`
- [ ] `ZMQ OS BUFFER = 4 * 1024 * 1024` (4MB send/recv) → `hft-zmq::defaults`
- [ ] `AGGREGATOR_MAX_DRAIN_BATCH = 512` → `publisher::aggregator`
- [ ] `AGGREGATOR_SMALL_SLEEP = 50us` (idle) → `publisher::aggregator`
- [ ] `LATENCY_SAMPLE_RATE = 1/100` → `hft-obs::sampler`
- [ ] `RECONNECT_INITIAL_DELAY = 1s`, `RECONNECT_MAX_DELAY = 30s`, multiplier `2x` → `hft-exchange-*`
- [ ] `SHM_HEADER_SIZE = 16`, `SHM_DIR_ENTRY = 16`, `SHM_MAX_SLOT = 512`, `SHM_MAX_SYMBOLS = 10_000` → `hft-shm` (deprecated — P1 에서는 crossbeam channel 로 대체)
- [ ] `FNV-1a init = 0xcbf29ce484222325`, `prime = 0x100000001b3` → `hft-shm::hash` (또는 삭제)
- [ ] `DUMPER_INTERVAL = 5ms` (SHM timestamp 갱신) — 삭제
- [ ] `METRICS_PRINT_INTERVAL = 5s` → `hft-obs::reporter`

---

## 1. `data_publisher_rust` — 멀티 거래소 WS → ZMQ 퍼블리셔

**→ target**: `services/publisher` + `crates/exchange/hft-exchange-{binance,gate,bybit,bitget,okx}`

### 1.1 CLI / env
- [ ] `--exchange` (default `binance`; 허용: binance/gate/bitget/bybit/okx) → `services/publisher::cli`
- [ ] `--workers` (default `15`) → `services/publisher::cli`
- [ ] `--backend` (optional, default 거래소별) → `services/publisher::cli`
- [ ] `--frontend` (optional, default 거래소별) → `services/publisher::cli`
- [ ] `--symbols` (default `common_symbols.txt`) → `services/publisher::cli`
- [ ] `--no-symbols-file` flag → `services/publisher::cli`
- [ ] `--log-level` (default `info`) → `services/publisher::cli` (hft-obs 가 실제 설정)
- [ ] `RUST_LOG` env → `hft-obs::init`
- [ ] `BINANCE_DATA_PORT=6000` / `GATE_DATA_PORT=5559` / `BITGET_DATA_PORT=6010` / `BYBIT_DATA_PORT=5558` / `OKX_DATA_PORT=6011` / `ZMQ_PORT=6000` (fallback) → `hft-core-config`
- [ ] `GATE_ORDERBOOK_PRECISIONS=gate_orderbook_precisions.txt` → `hft-exchange-gate`

### 1.2 Exchange WS 엔드포인트 (ExchangeFeed 구현체 별)
- [ ] Binance Futures `wss://fstream.binance.com/ws` — `bookTicker`, `aggTrade` 스트림 → `hft-exchange-binance`
- [ ] Gate API `wss://fx-ws.gateio.ws/v4/ws/usdt` — `futures.book_ticker`, `futures.trades` → `hft-exchange-gate`
- [ ] Gate Web `wss://fx-webws.gateio.live/v4/ws/usdt` — `futures.order_book` → `hft-exchange-gate` (role=WebOrderBook)
- [ ] Bitget `wss://ws.bitget.com/v2/ws/public` → `hft-exchange-bitget`
- [ ] Bybit `wss://stream.bybit.com/v5/public/linear` → `hft-exchange-bybit`
- [ ] OKX `wss://ws.okx.com:8443/ws/v5/public` → `hft-exchange-okx`
- [ ] 각 거래소별 subscribe payload JSON 포맷 (심볼 배열 / 채널명) → `hft-exchange-{v}::subscribe_payload()`
- [ ] 심볼 대소문자 변환 (Binance lowercase `btcusdt`, Gate/Bybit uppercase `BTC_USDT`) → `hft-exchange-{v}::to_wire_symbol()` / `from_wire_symbol()`

### 1.3 Worker (per WS connection)
- [ ] worker 수 `--workers` 분배 — 심볼 round-robin 샤딩 → `services/publisher::spawn_workers`
- [ ] worker 시작 stagger = `worker_id * 100ms` → `services/publisher::main`
- [ ] reconnect exponential backoff (1s → 2s → 4s → … → 30s cap) → `hft-exchange-{v}::stream`
- [ ] PUSH 소켓 연결 to `backend_addr`, SNDHWM=100k, OS send buf=4MB → `hft-zmq::push`
- [ ] `send_dontwait` 로 send — HWM 초과 시 drop count ↑ → `hft-zmq::push::try_send`
- [ ] frame 형식: `[type_len u8][type_str][payload]` — type_str ∈ {`bookticker`, `trade`, `webbookticker`} → `hft-protocol::frame`

### 1.4 Aggregator (central process)
- [ ] PULL `backend_addr` — default per exchange (`ipc:///tmp/{exchange}-bookticker` or `ipc:///tmp/hft_data_subscriber.sock` for gate) → `services/publisher::aggregator`
- [ ] PUB `frontend_addr` — TCP 0.0.0.0:{PORT}
- [ ] draw loop: drain up to 512 msg, coalesce by topic (HashMap<topic, (buf, dirty)>), publish dirty only → `publisher::aggregator::run`
- [ ] IPC 파일 기존 것 삭제 후 bind → `hft-zmq::bind_ipc`
- [ ] 5s 마다 stats print (latency p50/p95/p99/max, drop count) → `hft-obs::reporter`
- [ ] latency 측정: 샘플 1/100 에서 `publisher_sent_ms - server_time` 기록 → `hft-obs::sampler`

### 1.5 Wire format (C struct, little-endian, 정확한 offset)
- [ ] **BookTickerC 120B** — offset 검증 테스트 (`wire_compat.rs`) → `hft-protocol::BookTickerWire`
  - [ ] `exchange[16]` @0, `symbol[32]` @16, `bid_price:f64` @48, `ask_price:f64` @56, `bid_size:f64` @64, `ask_size:f64` @72
  - [ ] `event_time:i64` @80, `server_time:i64` @88, `publisher_sent_ms:i64` @96
  - [ ] `subscriber_received_ms:i64` @104, `subscriber_dump_ms:i64` @112 (publisher 에서는 0)
- [ ] **TradeC 128B** → `hft-protocol::TradeWire`
  - [ ] `exchange[16]` @0, `symbol[32]` @16, `price:f64` @48, `size:f64` @56
  - [ ] `id:i64` @64, `create_time:i64` @72 (sec), `create_time_ms:i64` @80
  - [ ] `is_internal:bool` @88, `_padding[7]` @89, `server_time:i64` @96, `publisher_sent_ms:i64` @104
  - [ ] `subscriber_received_ms:i64` @112, `subscriber_dump_ms:i64` @120 (publisher 에서는 0)
- [ ] Topic 문자열: `{exchange}_{type}_{symbol}` — e.g. `binance_bookticker_BTC_USDT`, `gate_webbookticker_BTC_USDT` → `hft-protocol::topics`
- [ ] encode < 200ns, decode < 300ns bench → `crates/core/hft-protocol/benches/`

### 1.6 Symbol 로딩
- [ ] `common_symbols.txt` 파일 로드 → `services/publisher::load_symbols`
- [ ] `--no-symbols-file` → 거래소 `available_symbols()` 만 사용
- [ ] 교집합 로직 (파일 ∩ 거래소 available) → `services/publisher::load_symbols`
- [ ] `IGNORE_SYMBOLS` 필터 → `hft-core-config::filter`

### 1.7 에러 / 리트라이
- [ ] WS disconnect → exponential backoff reconnect → `hft-exchange-{v}::stream`
- [ ] PUSH EAGAIN → log + drop count ↑ (block 하지 않음) → `hft-zmq::push`
- [ ] Aggregator bind 실패 → panic + clear error → `services/publisher::main`

---

## 2. `data_subscriber` — ZMQ → (legacy) SHM

**→ target**: `services/subscriber` (P1 에서는 SHM 제거, crossbeam channel 직결. SHM 은 Python strategy 호환용으로만 유지)

### 2.1 env / const
- [ ] `DATA_SERVER_IP=127.0.0.1` / `DATA_SERVER_IP_BACKUP` (failover) → `hft-core-config`
- [ ] `{EXCHANGE}_DATA_PORT` 전체 세트 (2.1 과 동일) → `hft-core-config`
- [ ] `SHM_PATH=/tmp/binance-cache.shm` → `services/subscriber::shm_bridge` (deprecated)
- [ ] `IPC_SOCKET_PATH=/tmp/hft_data_subscriber.sock` — Python strategy 호환 UDS → `services/subscriber::uds_bridge`
- [ ] `SUBSCRIBE_EXCHANGES` csv env → `hft-core-config`
- [ ] SUB RCVHWM = 100_000, RCVBUF = 4MB → `hft-zmq::sub`

### 2.2 기능
- [ ] SUB 연결 to `tcp://{DATA_SERVER_IP}:{PORT}` per exchange → `hft-zmq::sub`
- [ ] topic prefix subscription: `{exchange}_bookticker_*`, `{exchange}_webbookticker_*`, `{exchange}_trade_*` → `hft-protocol::topics`
- [ ] `poll` timeout 1000ms, idle sleep 10ms on empty → `services/subscriber::run`
- [ ] 메시지 카운터 per `{exchange}:{type}` 5초마다 출력 → `hft-obs::reporter`
- [ ] SHM 브리지 (Python 호환): FNV-1a hash + directory + 512B slot 쓰기 — dumper thread 5ms timestamp 갱신 → `services/subscriber::shm_bridge` (out of P1)
- [ ] SHM directory rebuild (데이터 있는데 dir 비어있으면 스캔) → 삭제
- [ ] UDS IPC bridge (Python ipc_client 호환): `[4B topic_len][topic][4B payload_len][payload]` + `REGISTER:<pid>:<sym1,sym2,…>\n` → `services/subscriber::uds_bridge`

### 2.3 P1 delta (새 아키텍처)
- [ ] crossbeam channel (bounded 8192) direct to strategy in-process → `services/subscriber::channel`
- [ ] hot path: SUB → decode → push to channel. 1 스레드. → `services/subscriber::run`
- [ ] SHM / UDS bridge 는 feature-flag `legacy-compat` 뒤로 → `services/subscriber::Cargo.toml`

---

## 3. `gate_hft_rust` — 전략 엔진 (5 binary)

**→ target**: `services/strategy/*` + `crates/strategy/hft-strategy-core` + `crates/exchange/hft-exchange-gate-exec`

### 3.1 Binary 별
- [ ] `bin/gate_hft_v1.rs` → `services/strategy/v1` (BasicStrategyCore)
- [ ] `bin/v6.rs` → `services/strategy/v6`
- [ ] `bin/v7.rs` (EMA profit) → `services/strategy/v7`
- [ ] `bin/v8.rs` (modified spread/gap) → `services/strategy/v8`
- [ ] `bin/close_v1.rs` → `services/strategy/close`

### 3.2 CLI / env (runner.rs)
- [ ] `--leverage` / `$LEVERAGE` → `hft-core-config`
- [ ] `--login_names` / `$LOGIN_NAMES` (csv or `prefix001~prefix010` range) → `services/strategy::cli::expand_logins`
- [ ] `--interval` (default 10ms) → `hft-core-config`
- [ ] `--binance-latency` / `--base-latency` (default 200ms) → `hft-core-config::TradeSettings`
- [ ] `--gate-latency` (default 100ms) → `hft-core-config::TradeSettings`
- [ ] `--restart-interval` (default 3600s) → `services/strategy::supervisor`
- [ ] `--initial-sleep` (default 100ms) → `services/strategy::main`
- [ ] `--base-exchange` / `$BASE_EXCHANGE` (default `binance`) → `hft-core-config`
- [ ] `$SUPABASE_URL`, `$SUPABASE_KEY` → `hft-core-config::supabase`
- [ ] `$GATE_API_KEY`, `$GATE_API_SECRET` → `hft-exchange-gate-exec`
- [ ] `$LOGIN_NAME` (default `v3_sb_002`), `$UID` → `hft-core-config`
- [ ] `$SYMBOLS` csv → `hft-core-config`
- [ ] `$METRICS_DIR` (default `./metrics`) → `hft-obs::metrics_writer`
- [ ] `$HEALTHCHECK_TEST_ORDER_{SYMBOL,SIZE,PRICE=60000.0}` → `services/strategy::healthcheck`
- [ ] `$LOGIN_STATUS_URL` → `services/strategy::healthcheck`
- [ ] `$ORDER_URL`, `$ORDER_UID` (puppeteer) → `hft-exchange-gate-exec::puppeteer`
- [ ] `$IPC_SOCKET_PATH=/tmp/gate_hft_ipc.sock` → `services/subscriber::uds_bridge`
- [ ] `$REDIS_URL`, `$REDIS_STATE_PREFIX=gate_hft:state` → `hft-state::redis`
- [ ] `$SUPABASE_REFRESH_SECS=300` → `hft-core-config::supabase`

### 3.3 Supabase 설정 로딩 순서
- [ ] `strategy_settings` by `login_name` → `trade_setting` (text FK), `symbol_set` (bigint FK) → `hft-core-config::supabase::load_strategy`
- [ ] `trade_settings` by id → JSON → `TradeSettings` struct
- [ ] `symbol_sets` by id → JSON array → `Vec<Symbol>`
- [ ] startup-only load (런타임 reload 없음) — 주기적 반영은 `SUPABASE_REFRESH_SECS` 로 in-memory 업데이트 → `hft-core-config::supabase::refresher`

### 3.4 TradeSettings 필드 (모두 Supabase JSON 에서 로드)
Core:
- [ ] `update_interval_seconds` (default 0.1)
- [ ] `order_size: f64` (USDT)
- [ ] `close_order_size: Option<f64>`
- [ ] `max_position_size: f64`
- [ ] `dynamic_max_position_size: bool` (default false)
- [ ] `max_position_size_multiplier: f64` (default 1.5)
- [ ] `trade_size_trigger: f64`
- [ ] `max_trade_size_trigger: Option<f64>`
- [ ] `close_trade_size_trigger: Option<f64>`
- [ ] `close_order_count: i64`
- [ ] `max_order_size_multiplier: i64` (default 3)

Profit / gap:
- [ ] `close_raw_mid_profit_bp: f64`
- [ ] `ignore_recent_trade_ms: i64` (default 0)
- [ ] `mid_gap_bp_threshold: f64`
- [ ] `binance_mid_gap_bp_threshold: f64` (default 0.0)
- [ ] `spread_bp_threshold: f64`

Latency gating:
- [ ] `gate_last_trade_latency_ms: i64` (default 1000)
- [ ] `gate_last_book_ticker_latency_ms: i64`
- [ ] `binance_last_book_ticker_latency_ms: i64`
- [ ] `gate_last_webbook_ticker_latency_ms: i64` (default 500)

Order management:
- [ ] `wait_time_close_ws_order_ms: i64` (default 1000)
- [ ] `bypass_safe_limit_close: bool` (default false)
- [ ] `only_follow_trade_amount: bool` (default true)
- [ ] `allow_limit_close: bool` (default true)
- [ ] `ignore_non_profitable_order_rate: f64` (default 0.6)
- [ ] `order_success_rate: f64` (default 0.05)
- [ ] `bypass_max_position_size: bool` (default false)
- [ ] `opposite_side_max_position_size: Option<f64>`

TIF:
- [ ] `limit_open_tif: String` (default `fok`)
- [ ] `limit_close_tif: String` (default `fok`)

Time restrictions (ms min / max):
- [ ] `limit_open_time_restriction_ms_min=100`, `_max=200`
- [ ] `limit_close_time_restriction_ms_min=100`, `_max=200`
- [ ] `same_side_price_time_restriction_ms_min=300`, `_max=600`
- [ ] `wait_for_book_ticker_after_trade_time_ms: i64` (default 100)

Funding / EMA / misc:
- [ ] `funding_rate_threshold: f64` (default 0.005, or 0.001 in Python — confirm)
- [ ] `profit_bp_ema_alpha: f64` (default 0.1)
- [ ] `profit_bp_ema_threshold: f64` (default 1.20)
- [ ] `succes_threshold: f64` (default 0.6) — ⚠️ 철자 그대로 유지
- [ ] `normalize_trade_count: i64` (default 100)
- [ ] `close_stale_minutes: i64` (default 99999)
- [ ] `close_stale_minutes_size: Option<f64>`
- [ ] `too_many_orders_time_gap_ms: i64` (default 600s)
- [ ] `too_many_orders_size_threshold_multiplier: i64` (default 2)
- [ ] `symbol_too_many_orders_size_threshold_multiplier: i64` (default 2)
- [ ] `net_positions_usdt_size_threshold: f64` (default 5000.0)
- [ ] `max_order_size`: derived `max_position_size * max_order_size_multiplier`

### 3.5 Signal 계산 (signal_calculator.rs)
- [ ] `gate_mid = (gate_ask + gate_bid) / 2` → `hft-strategy-core::signal`
- [ ] `buy_price = gate_web_ask`, `sell_price = gate_web_bid`
- [ ] buy 조건: `buy_price < gate_mid` → side=buy, `mid_gap_bp = (gate_mid - buy_price) / gate_mid * 10_000`, `orderbook_size = gate_web_ask_size`, binance valid: `buy_price < binance_bid`
- [ ] sell 조건: `sell_price > gate_mid` → side=sell, `mid_gap_bp = (sell_price - gate_mid) / gate_mid * 10_000`, `orderbook_size = gate_web_bid_size`, binance valid: `sell_price > binance_ask`
- [ ] `spread_bp = (gate_ask - gate_bid) / gate_mid * 10_000`
- [ ] `is_binance_valid` 계산
- [ ] `instant_profit_bp` (buy: `(bid-trade_price)/trade_price*10_000`, sell: `(trade_price-ask)/trade_price*10_000`) → `hft-strategy-core::profit`

### 3.6 Order Decision (order_decision.rs)
- [ ] latency guard: now - gate_bt_time > threshold → skip
- [ ] binance latency guard 동일
- [ ] funding rate gating: buy + funding > +threshold → only_close; sell + funding < -threshold → only_close
- [ ] **limit_open** 조건 (all true): mid_gap_bp > threshold, size ∈ [trigger, max_trigger], spread > threshold, binance_valid, funding not blocking
- [ ] **limit_close** 조건: mid_gap_bp > close_raw_mid_profit, size > close_trigger, order_count_size > close_order_count, binance_mid_gap ≥ 0
- [ ] only_close → limit_close 로 downgrade
- [ ] position 반대방향 확인 (close 은 long→sell, short→buy)
- [ ] TIF 는 trade_settings 에서 가져오기

### 3.7 Risk Manager (risk_manager.rs / handle_chance.rs)
- [ ] `MAX_POSITION_SIDE_RATIO = 0.5` → `hft-strategy-core::risk`
- [ ] `DEFAULT_LEVERAGE = 50.0` (risk_manager) / `1000.0` (handle_chance) → `hft-strategy-core::risk`
- [ ] net exposure check: `|net_size| * price / leverage < MAX_POSITION_SIDE_RATIO * balance`
- [ ] per-symbol max position size check (with multiplier)
- [ ] `too_many_orders` rate-limit check (time_gap_ms window)

### 3.8 Gate Exec
- [ ] REST base: `https://api.gateio.ws/api/v4/futures/usdt` → `hft-exchange-gate-exec`
- [ ] HMAC-SHA256 signing (API key + secret)
- [ ] `POST /orders` with `{account, contract, price(str), reduce_only, order_type, size(str, ±), tif, text, message.timestamp}`
- [ ] `DELETE /orders/{id}` cancel
- [ ] `GET /positions`, `GET /orders`, `GET /my_trades`, `GET /accounts`
- [ ] WS user stream: order updates, position updates → `hft-exchange-gate-exec::user_stream`
- [ ] Puppeteer fallback: POST `{ORDER_URL}` with same payload — for browser-based orders → `hft-exchange-gate-exec::puppeteer`

### 3.9 Health / Supervisor
- [ ] `DEBUGGING_ORDERS = [{contract:"BTC_USDT", price:"60000"}, {contract:"ETH_USDT", price:"2000"}]` — healthcheck 테스트 주문 → `services/strategy::healthcheck`
- [ ] `strategy_status` supabase table upsert (columns: login_name, instance_name, ip_address, is_healthy, is_logged_in, too_many_requests, browser_connected, health_check_order_response) → `services/strategy::status`
- [ ] restart loop: every `restart_interval` seconds → `services/strategy::supervisor`
- [ ] metrics files per-strategy in `$METRICS_DIR` → `hft-obs::metrics_writer`

---

## 4. `futures_collector` — 심볼 메타데이터 수집

**→ target**: `tools/futures-collector`

- [ ] `--symbol` required arg → `tools/futures-collector::cli`
- [ ] `$GATE_ORDERBOOK_PRECISIONS` env → `hft-exchange-gate`
- [ ] WS: Gate web bookticker, Binance futures
- [ ] Parquet Hive partition `{out}/{symbol}/year=/month=/day=/hour=/`
- [ ] BookTickerRow / TradeRow schema
- [ ] reconnect backoff 30s max
- [ ] (Rewrite) 각 거래소 `ExchangeFeed::fetch_symbols()` 재사용 → 모든 거래소 심볼 upsert to supabase `symbols` table
- [ ] cron `*/10 * * * *` or systemd timer 단발 실행
- [ ] 거래소 중 1개 실패해도 다른 거래소 계속

---

## 5. `latency_test_subscriber` — 디버깅 유틸

**→ target**: `tools/latency-test-subscriber`

- [ ] `$DATA_SERVER_IP`, `${EXCHANGE}_DATA_PORT` env
- [ ] SUB RCVHWM 100k, RCVBUF 4MB
- [ ] BookTickerC 120B / TradeC 128B decode
- [ ] latency 계산: `publisher_sent_ms - server_time` (server→pub), `now - publisher_sent_ms` (pub→here)
- [ ] HDR histogram percentile 출력 → `hft-obs::histogram`

---

## 6. `questdb_export` — QuestDB SUB writer + export

**→ target**: `tools/questdb-export`

### 6.1 Runtime (SUB → ILP writer)
- [ ] SUB subscribe `""` (모든 토픽) → `tools/questdb-export`
- [ ] 100ms batch flush
- [ ] SIGTERM graceful: drain + `force_flush`
- [ ] QuestDB PG wire (8812) schema init on startup
- [ ] ILP retry + spool file on connect failure (publisher 에 backpressure 없음)
- [ ] `latency_stages` 테이블 — LatencyStamps 7 필드 모두 기록

### 6.2 Export CLI (bin `questdb-export-cli`?)
- [ ] `--url` / `$QUESTDB_URL`
- [ ] `--table`, `--query`, `--output`, `--output-dir=data`
- [ ] `--partition-by` (hour | day, default day)
- [ ] `--limit`
- [ ] `--symbol`, `--start-date`, `--end-date`, `--start-ts`, `--end-ts` (RFC3339)
- [ ] HTTP API: `{QUESTDB_URL}/exec?query=`
- [ ] Parquet single-file / multi-file hourly partition

---

## 7. `order_processor_go` — 브라우저 자동화 HTTP 서버

**→ target**: `go/order-processor` (이미 포팅되어 있으면 기능 parity 확인)

- [ ] `$PORT=3005`, `$RATE_LIMIT=20`, `$CURRENT_IP=127.0.0.1` env
- [ ] HTTP routes
  - [ ] `POST /puppeteer/simple-order`
  - [ ] `GET /puppeteer/account-status/{accountId}`
  - [ ] `GET /puppeteer/login-status`
  - [ ] `GET /puppeteer/qr/{accountId}`
  - [ ] `POST /puppeteer/refresh/{accountId}`
  - [ ] `GET /puppeteer/health`
  - [ ] `GET /puppeteer`
  - [ ] `GET /puppeteer/check-login-env`
  - [ ] `GET /static/*`, `GET /`
- [ ] HTTP server timeouts: Read=10s, ReadHeader=2s, Write=30s, Idle=120s, MaxHeader=1MB
- [ ] Token bucket rate limiter: `RATE_LIMIT` rps / burst
- [ ] Chrome/ChromeDP headless, user-data-dir per account
- [ ] cookie jar 관리
- [ ] cache: URL / cookies / login status / login result (timestamped)
- [ ] `CleanupLeftoverChromeProcesses()` on startup
- [ ] graceful shutdown 30s timeout

---

## 8. `data_publisher_gate_ws_go` — Go 대체 Gate 퍼블리셔

**→ target**: ⏸ **Deferred** — Rust publisher 가 Gate 지원하므로 포팅 안 함. 기능 parity 만 검증.

- [ ] WS `wss://fx-ws.gateio.ws/v4/ws/usdt` 구독 (`futures.book_ticker`, `futures.trades`)
- [ ] reconnect 1s→30s exp
- [ ] keep-alive: 30s ping / 60s read deadline
- [ ] ZMQ PUB out (Rust publisher 와 동일 wire format)

---

## 9. `gate_hft_v29.py` — 최신 Python 전략 (parity 검증용)

**→ target**: `services/strategy/*` (이미 Rust binaries 가 커버). Python 은 레퍼런스로만.

### 9.1 CLI
- [ ] `--debug`
- [ ] `--login_names` (csv or `prefix001~prefix010` range)
- [ ] `--symbol_set` int
- [ ] `--close` flag (close mode)
- [ ] `--subaccount_keys_path=subaccount_keys.json`

### 9.2 env (중복 제외한 Python 전용)
- [ ] `$INSTANCE_NAME` — server 식별자
- [ ] `$QDB_CLIENT_CONF` — QuestDB sender conf

### 9.3 핵심 아키텍처
- [ ] 이벤트-기반 IPC (UDS blocking I/O — OS wake on arrival)
- [ ] C Struct zero-copy deserialize (BookTickerC 128B Python version — `subscriber_*_ms` 포함, Rust 의 120B + 16B)
- [ ] RWLockFair 로 `_binance_booktickers / _gate_booktickers / _gate_webbooktickers / _gate_trades` Dict 보호
- [ ] `_binance_received_at / _gate_bookticker_received_at / _gate_webbookticker_received_at / _gate_trade_received_at` Dict
- [ ] `_symbol_last_processed_time` Dict + `_min_processing_interval_ms = 1` debounce
- [ ] `_order_queue = Queue(maxsize=5000)` + ThreadPoolExecutor(`min(16, accounts*2)`)
- [ ] pre-warmup 10s sleep + force close non-strategy positions on startup

### 9.4 Strategy variants (포팅 필요 여부 표기)
- [ ] `gate_hft_v29.py` — 최신 기준 → ✅ Rust v8 (추정) 에 매핑
- [ ] `gate_hft_v28.py / v28.1 / v28.safe` → `services/strategy/v8` 포팅 완료 시 v28 파이프라인 deprecated
- [ ] `gate_hft_v27.x` 시리즈 → ⏸ deprecated (v29 가 커버)
- [ ] `gate_hft_v26.x` 시리즈 → ⏸ deprecated
- [ ] `gate_hft_v25_subscriber_test.py` → ⏸ test/demo, drop
- [ ] `gate_hft_test.py`, `gate_hft_test_zmq.py`, `test_sub.py`, `gate_hft_v26.explorer.py` → ⏸ drop

### 9.5 Close / MM 보조 전략
- [ ] `gate_close_strategy_v1.py` — OrderManagerV6 + `slippage=1.0%` → `services/strategy/close`
- [ ] `gate_hft_mm_close.py` — MM close tight spread (POC TIF) → `services/strategy/mm_close` (P2)
- [ ] `gate_hft_close_unhealthy_symbols.py` — stale / unhealthy 심볼 강제 종료 → `services/strategy/close_unhealthy` (P2)

---

## 10. Python `src/` 모듈 (strategy 내부 컴포넌트)

**→ target**: 각 기능을 `hft-strategy-core` / `hft-exchange-gate-exec` / `hft-account` / `hft-obs` 로 분해.

### 10.1 Order Managers
- [ ] `src/order_manager_v3.py` → `hft-exchange-gate-exec::OrderManagerV3` 동등 기능
  - [ ] `list_positions(settle=usdt)`, `get_futures_account()`, `list_futures_orders()`, `list_futures_trades()`, `create_order()`, `cancel_order()`, `update_positions()`, `calculate_balance()`
  - [ ] `UserTrade` model (id, create_time_ms, contract, order_id, size±, price, role maker/taker, text, fee, point_fee, side property)
  - [ ] Position model (contract, size, entry_price, realised_pnl, last_close_pnl, history_pnl, mark_price, liquidation_price)
  - [ ] order param: {account, contract, price(str), reduce_only, order_type(limit|market), size(str ±), tif(ioc|fok|gtc), text(web), message.timestamp}
  - [ ] fee calc: `paid_fee_ratio=0.35`, `fee_bp=3`
  - [ ] instant profit bp calc (buy/sell)
  - [ ] order success/profitable rate tracking
  - [ ] WS reconnect: 1s→30s exp, ping 2.5s, ping timeout 5s, max_attempts=10
- [ ] `src/order_manager_v2.py / v4.py / v5.py` — ⏸ deprecated (v3/v6 로 수렴)
- [ ] `src/order_manager_v6.py` — v3 + slippage param → `services/strategy/close`
- [ ] `src/order_manager_mm.py` — MM-optimized symmetric bid/ask → `hft-strategy-mm` (P2)
- [ ] `src/order_manager.py` — v1 legacy ⏸ drop

### 10.2 Market Watchers
- [ ] `src/market_watchers/base_market_watcher.py` → `hft-exchange-api::ExchangeFeed` trait (이미 계획됨)
- [ ] `src/market_watchers/binance_market_watcher_v5.py` → `hft-exchange-binance`
  - [ ] ping interval 2s, timeout 5s, high_performance=true
  - [ ] `max_subs_per_stream=200`, `stream_buffer_maxlen=2` FIFO, `batch_max_ms=5.0`, `warn_stale_ms_book=200`
  - [ ] `_max_backoff_sec=60`, `delay = min(2^attempts, max_backoff_sec)` + `random.uniform(0, max(0.5, 0.5*delay))` jitter
  - [ ] `last_book_ticker_updated_at`, `last_book_ticker_server_timestamp_ms`, `is_running` (recent 1s + 500ms)
  - [ ] channels: `depth5@100ms`, `bookTicker`
  - [ ] symbol 변환: `to_binance_symbol()` / `from_binance_symbol()`
- [ ] `src/market_watchers/binance_market_watcher{,_v2,_v3,_v4}.py` / `binance_market_watcher_ws.py` ⏸ deprecated
- [ ] `src/market_watchers/gate_market_watcher_v3.py` → `hft-exchange-gate` (user stream + public channels)
- [ ] `src/market_watchers/gate_market_watcher{,_v2}.py`, `gate_market_watcher_zmq.py` ⏸ deprecated
- [ ] `src/market_watchers/bybit_market_watcher.py` → `hft-exchange-bybit`
- [ ] `src/market_watchers/okx_market_watcher.py` → `hft-exchange-okx`

### 10.3 Models
- [ ] `src/models/book_ticker.py` → `hft-types::BookTicker`
- [ ] `src/models/ticker.py` → `hft-types::Ticker`
- [ ] `src/models/settings.py::GateHFTV21TradeSettings` → `hft-core-config::TradeSettings` (전체 필드 4.3.4 참조)
- [ ] `src/models/settings.py::StrategySetting` loader → `hft-core-config::supabase`

### 10.4 Callback Handlers
- [ ] `src/callback_handlers/base_callback_handler.py` → `hft-ipc::EventSink` (trait)
- [ ] `src/callback_handlers/print_callback_handler.py` → `hft-obs::stdout_sink`
- [ ] `src/callback_handlers/questdb_callback_handler.py` → `tools/questdb-export` 의 sink 로 흡수

### 10.5 Utils
- [ ] `src/utils/shm_reader.py` → `hft-shm` (legacy compat, P1 외)
  - [ ] BookTickerC 128B / TradeC 160B Python 버전 (⚠️ Rust 의 120B / 128B 와 다름 — dump_ms 필드 포함 여부 확인)
  - [ ] FNV-1a hash 함수
  - [ ] composite key `{exchange}:{type}:{symbol}`
  - [ ] linear probing + `composite_key_to_slot` cache
- [ ] `src/utils/ipc_client.py` → `hft-ipc::UdsClient` (Python compat)
  - [ ] UDS `/tmp/hft_data_subscriber.sock`
  - [ ] message frame `[4B topic_len][topic][4B payload_len][payload]`
  - [ ] register `REGISTER:<pid>:<symbols>\n`
  - [ ] `Queue(maxsize=10000)` fallback
  - [ ] `received_at_ms` timestamp
- [ ] `src/utils/timing.py` → `hft-time::{get_ts, get_ts_ms, get_common_symbols}`
- [ ] `src/utils/subaccount_utils.py::get_ip()` (error-msg 추출) → `hft-account::ip_detector`
- [ ] `src/utils/subaccount_utils.py::SubaccountUtils` → `hft-account::SubaccountOps`
- [ ] `src/utils/mean_timing.py` → `hft-obs::rolling_mean`
- [ ] `src/utils/flatbuffer_helpers.py` → ⏸ flatbuffer 사용 안 함, drop

### 10.6 Ingest / Monitoring / Strategy
- [ ] `src/ingest/fusion_ingestor.py` → (필요 시) `services/fusion-ingestor`. 현재 QuestDB sink 로 충분하면 drop
- [ ] `src/monitoring/monitoring_agent.py` → `services/monitoring-agent` (P3)
- [ ] `src/strategy_manager.py` → `services/strategy::manager`
- [ ] `src/account_manager.py` → `hft-account::AccountManager`
  - [ ] SubAccountKey (state, mode, name, user_id, perms, ip_whitelist, secret, key, created_at, updated_at)
  - [ ] MainAccountSecret (api_key, secret_key, uid, name)
  - [ ] `sub_account_keys_path=subaccount_keys.json`, `main_account_secrets_path=main_account_secrets.json`
  - [ ] `get_sub_account_uid_from_login_name()`, `get_sub_account_key_from_login_name()`, `get_sub_account_available_balance()`, `is_sub_account_prepared()`, `create_sub_account_key_if_not_prepared()`
  - [ ] caches: `sub_account_uid_to_login_name`, `sub_account_uid_to_subaccount`
  - [ ] IP whitelist 체크 (get_ip from API error)
- [ ] `src/margin_manager.py` + `margin_manager_v2.py` → `hft-account::MarginManager`
  - [ ] `StrategySubAccountFuturesBalance` wrapper (balance, historical_estimated_profit, unrealised_pnl, history, fee, estimated_trading_volume)
  - [ ] `paid_fee_ratio=0.35`, `fee_bp=3`
  - [ ] `start_monitoring()`, `start_balancing()` async loops
  - [ ] regex login filter (`v2_jw_\d+` 류)
  - [ ] liquidation risk levels: critical (<5%), warning (5-10%), safe (>10%)
  - [ ] margin target configurable (e.g., 1000 USDT/account)
- [ ] `src/error_manager.py` → `services/error-manager` (P3)
  - [ ] check_interval=60s, alert_threshold=30min
  - [ ] `telegram_subscribers.json`, `main_accounts.json`
  - [ ] Telegram bot (`$TELEGRAM_BOT_TOKEN`, commands `/start`/`/stop`/`/status`, HTML emoji)
  - [ ] `initial_fees` dict (inactive), `fee_tracker` dict (active, alerted flag)
- [ ] `src/time_bucket_td_buffer.py` → `hft-strategy-core::buffer` (time-bucketed trade data buffer)

---

## 11. 탑레벨 Python ops 스크립트

**→ target**: 대부분 ⏸ **Python 에서 그대로 운영**. 일부만 Rust CLI 로 이식.

### 11.1 계정 운영
- [ ] `check_futures_mode.py` — futures 모드 enabled 확인 → ⏸ Python 유지
- [ ] `close_positions.py` — emergency close-all → 🔶 `tools/close-positions` (P2)
- [ ] `close_all_and_send_to_main_account.py` → ⏸ Python 유지
- [ ] `convert_subaccounts.py` → ⏸ Python 유지
- [ ] `get_subaccount.py`, `get_subaccount_keys.py`, `get_subaccount_positions_pnl.py`, `get_subaccount_trades_summary.py` → ⏸ Python 유지
- [ ] `send_balance_to_main_account.py`, `transfer_to_main_account.py` → ⏸ Python 유지
  - [ ] Gate API `transfer_with_sub_account(sub_account, sub_account_type=futures, currency=USDT, amount, direction)` 사용
- [ ] `sort_subaccount_trades.py` → ⏸ Python 유지
- [ ] `generate_accounts_json.py` → ⏸ Python 유지
- [ ] `generate_available_trading_pairs.ipynb`, `generate_backtesting_data.py`, `generate_trading_pairs_batches.py`, `get_common_symbols.ipynb`, `common_symbols.txt`, `find_unused_symbols_and_update_supabase.ipynb`, `trading_pairs_batches.json` → ⏸ Python/notebook 유지

### 11.2 헬스 / 오케스트레이션
- [ ] `gate_hft_healthcheck.py` → 🔶 `tools/healthcheck` (P2)
  - [ ] QuestDB PG (host=$QUESTDB_HOST, port=8812, db=questdb, user=admin, pw=quest)
  - [ ] `SELECT DISTINCT symbol FROM gate_webbookticker where timestamp > dateadd('h',-1,now())`
- [ ] `gate_hft_status_check.py` → 🔶 `tools/status-check` (P2)
- [ ] `restart_gate_hft.py` → `services/strategy::supervisor` (내재화, CLI: `--restart-interval`, `--overlap`) (P2)
- [ ] `monitoring_v2.py` → 🔷 `services/monitoring` (P3) — subaccount 잔고/마진/liquidation 감시
  - [ ] accounts dict (monitoring_targets regex list, main_account_name)
  - [ ] async loop, `asyncio.sleep(2)`
  - [ ] long/short totals
- [ ] `monitoring_questdb.py` → 🔷 `tools/questdb-monitor` (P3)
- [ ] `ws_questdb_monitoring.py` → 🔷 `tools/ws-monitor` (P3)

### 11.3 QuestDB / 데이터 ingress
- [ ] `quest_db_ingressor.py` → 흡수됨 (`tools/questdb-export` 가 동일 역할)
  - [ ] 테이블: `gate_bookticker`, `gate_trade`, `binance_bookticker` 등
  - [ ] `Sender.from_conf($QDB_CLIENT_CONF)` ILP
- [ ] `zmq_to_dict_subscriber.py` → ⏸ 디버그용, drop
- [ ] `external_latency_test.py` → 흡수 (`tools/latency-test-subscriber`)
- [ ] `update_binance_bookticker_to_questdb.py` → ⏸ 일회성 migration, drop

### 11.4 준비 / 배포
- [ ] `prepare_strategy_v2.py` → 🔶 `tools/prepare-strategy` (P2)
  - [ ] CLI: `--main_account`, `--login_name_prefix`, `--initial_margin_per_account=1000`, `--start_index=1`, `--end_index=100`, `--send_balance`, `--update_supabase`, `--trade_settings=v18.md`, `--symbol_set_offset=0`
  - [ ] subaccount 없으면 생성
  - [ ] initial margin 전송
  - [ ] Supabase `strategy_settings` 등록
  - [ ] key 생성: `name=trade_{yyyymmdd_hhmm}`, perms=[futures, spot], ip_whitelist 자동 수집
- [ ] `prepare_strategy_temp.py` → ⏸ drop
- [ ] `profit_calculator.py` → 🔷 `tools/profit-calc` (P3)
- [ ] `margin_manager.py` (top-level) → `hft-account::MarginManager` 와 동등

### 11.5 거래소별 WS 스트림 (Python standalone)
- [ ] `gate_bookticker_ws_stream.py` — Gate WS → Parquet Hive
  - [ ] `WS_URL_MAP = {web, api, trade}`, subscription `futures.book_ticker`, `BATCH_SIZE=1000`
  - [ ] partition `data/exchange=gatefutures/symbol=.../date=.../hour=.../`
- [ ] `gate_order_book_ws_stream.py` — `futures.order_book`
- [ ] `gate_trades_ws_stream.py` — `futures.trades`
- [ ] `gate_fx_ws_stream.py` — Gate FX
- [ ] `binance_publisher.py` / `binance_subscriber.py` — 🔶 Rust publisher 로 완전 대체
- [ ] `bingx_bookticker_stream.py` + `bingx_collector.py` → 🔷 `hft-exchange-bingx` (P3 if needed)
- [ ] `bitget_bookticker_stream.py` + `bitget_collector.py` → ✅ `hft-exchange-bitget` (P1)
- [ ] `bybit_bookticker_ws_stream.py` → ✅ `hft-exchange-bybit` (P1)

### 11.6 Data collectors
- [ ] `my_trades_collector.py` — 개인 거래 내역 수집 → 🔷 `tools/my-trades-collector` (P3)
- [ ] `local_tick_collector.py` — 로컬 틱 수집 → 🔷 `tools/tick-collector` (P3)
- [ ] `data/downloader.py` → ⏸ Python 유지

### 11.7 Scripts 기타
- [ ] `benchmark_shm.py` → ⏸ drop (SHM deprecated)
- [ ] `binary_serialization_examples.py`, `binary_serialization_implementation.py`, `binary_serialization_options.md` → reference for `hft-protocol`
- [ ] `check_accounts.ipynb` → ⏸ Python 유지
- [ ] `unicorn-binance-pip.sh`, `unicorn-binance-uv.sh` → ⏸ install script, drop (uv/pip 사용 안 함)
- [ ] `QUICK_LXC_SETUP.sh`, `EXTRACT_FROM_DOCKER_FOR_LXC.md`, `PROXMOX_LXC_DOCKER_SETUP.md`, `PROXMOX_TROUBLESHOOTING.md` → 🔷 `mono/deploy/` 에 재정리
- [ ] `settings.v16.sb.v3.json` → 레퍼런스, `hft-core-config` defaults 로 흡수
- [ ] `profit_baseline_sb_2025-12-10_*.json` → 레퍼런스 스냅샷

---

## 12. `schemas/` — FlatBuffer / Protobuf

**→ target**: `hft-protocol` 이 C-struct wire 로 통일 → FlatBuffer / Protobuf 는 드랍 (내부 ZMQ 전송에서 사용 안 함).

- [ ] `schemas/book_ticker.fbs`, `market_data.fbs`, `trade.fbs`, `processed_data.fbs` → ⏸ drop
- [ ] `schemas/processed_data.proto` → ⏸ drop
- [ ] FlatBuffer table 필드 목록 확인 (참조용):
  - BookTicker: e, s, bid_price, ask_price, bid_size, ask_size, event_time, server_time, publisher_sent_ms, subscriber_received_ms, subscriber_dump_ms
  - Trade: e, s, size(signed), id, create_time, create_time_ms, price, is_internal, server_time, publisher_sent_ms, subscriber_received_ms, subscriber_dump_ms
  - ProcessedData: symbol, processor_time_ms, GateTrade, GateBookTicker, BinanceBookTicker
- [ ] 필드 parity 를 `hft-protocol` SPEC 의 BookTickerWire / TradeWire 와 1:1 대응하는지 확인

---

## 13. `monitoring/` (Docker)

**→ target**: `mono/deploy/` (P3)

### 13.1 QuestDB
- [ ] ports 8812 (pg), 9000 (http), 9009 (ilp/tcp)
- [ ] 2 CPU, 16GB RAM
- [ ] JVM `-Xms4g -Xmx8g -XX:+UseG1GC`
- [ ] worker 4 http / 4 pg / 4 sql / 2 wal
- [ ] WAL enabled, segment 1GB, commit_lag 50s, max_page_rows 100K
- [ ] TCP 9009 healthcheck 30s/10s

### 13.2 Grafana
- [ ] port 3000, 1 CPU / 8GB
- [ ] default password `admin`

### 13.3 Redis
- [ ] port 6379, 1 CPU / 2GB
- [ ] AOF enabled, redis 7.4-alpine
- [ ] PING healthcheck 10s/5s

### 13.4 Prometheus / PushGateway (commented out)
- [ ] port 9090 / 9091, retention 30d
- [ ] → 활성화 여부 결정 (Phase 3)

### 13.5 monitoring/prometheus.yml, monitoring/grafana/, PUSHGATEWAY_SETUP.md
- [ ] scrape config 이관 → `mono/deploy/monitoring/`

---

## 14. `analysis/` — 분석 / 백테스팅

**→ target**: 🔷 `mono/analysis/` 또는 ⏸ Python 유지

- [ ] `analyze_results.py`, `calculate_hourly_pnl.py`, `collect_crypto_data.py`, `compare_strategy_performance.py`, `generate_trading_report.py`, `optimize_params.py` → ⏸ Python 유지
- [ ] `analyze_crypto_data.ipynb`, `analyze_futures_data.ipynb`, `analyze_tick_data.ipynb`, `backtest.ipynb` → ⏸ notebook 유지
- [ ] `analysis/report/` → 출력 아티팩트, 무관

---

## 15. `strategy/rl` — 강화학습 실험

**→ target**: ⏸ **Out of scope**. 별도 트랙. `mono/` 로 이식 안 함.

- [ ] RL 실험 코드 전체 — drop from scope

---

## 16. `sigma_work/` (아키텍처 문서 / SSH 키)

**→ target**: ⏸ drop (SSH 키는 레포에서 제거 권장)

- [ ] `sigma_work/id_ed25519`, `sigma_work/id_ed25519.pub` — ⚠️ **SECURITY: 반드시 레포에서 삭제 + 키 로테이션**
- [ ] `sigma_work/ARCHITECTURE_REDESIGN.md`, `DEPLOYMENT_GUIDE.md`, `REPOSITORY_ANALYSIS.md` → docs/adr 로 흡수 여부 결정

---

## 17. 루트 문서 / 기타

- [ ] `README.md`, `README.v18.md`, `STRATEGY_LOGIC_v21.5.md` → `mono/docs/adr/` 로 레퍼런스 이동
- [ ] `pyproject.toml`, `uv.lock`, `requirements.txt` — Python 런타임 유지 시 보존
- [ ] `gate_orderbook_precisions.txt` → `hft-exchange-gate` 에 포함 (정적 데이터)
- [ ] `gate_hft.log` → .gitignore 등재

---

## 18. Cross-cutting Phase 1 블로커 (check.md 외부 의존성)

Phase 1 완료 조건에 직접 연관:

- [ ] `hft-types` — ExchangeId/Symbol/Price/Size (mimalloc allocator 포함) 완성 후 체크
- [ ] `hft-protocol` — 120B/128B wire format + `tests/wire_compat.rs` 통과 후 체크
- [ ] `hft-time` — MockClock + LatencyStamps 7-stage 완성 후 체크
- [ ] `hft-obs` — HDR histogram + non-blocking tracing 완성 후 체크
- [ ] `hft-core-config` — Figment layered + Supabase refresh 완성 후 체크
- [ ] `hft-zmq` — PUB/SUB/PUSH/PULL 래퍼 + SendOutcome (WouldBlock/Sent/Error) 완성 후 체크
- [ ] `hft-storage` — QuestDB ILP Sink + spool 완성 후 체크
- [ ] `hft-exchange-api` — Feed/Executor trait 완성 후 체크
- [ ] `hft-exchange-binance/gate/bybit/bitget/okx` — 각 SPEC 의 완료 조건 충족 후 체크
- [ ] `hft-testkit` — MockFeed + WarmUp + pipeline_harness! 완성 후 체크
- [ ] `services/publisher` — worker+aggregator + patch_stamp + 100K msg/s p99.9<2.1ms 달성 후 체크
- [ ] `services/subscriber` — crossbeam channel + UDS/SHM bridge(feature-gated) 완성 후 체크
- [ ] `tools/questdb-export`, `tools/futures-collector`, `tools/latency-test-subscriber` 각 SPEC 완료 후 체크
- [ ] `crates/testing/integration` — 6개 e2e 테스트 통과 후 체크 (특히 `e2e_pipeline_mock.rs` 의 stage p99.9)
- [ ] 전체 `cargo test --workspace` 녹색
- [ ] `make fmt lint` 통과

---

## Phase 매핑 요약

| Phase | 범위 | 포함 섹션 |
|-------|------|-----------|
| ✅ **P1** | publisher + subscriber + storage + exchange feed × 5 + latency infra | §1, §2 (legacy bridge 제외), §4, §5, §6.1, §18 |
| 🔶 **P2** | strategy 엔진 (v6/v7/v8/close) + 계정/마진/헬스 | §3, §9, §10.1-10.4, §10.6, §11.2 (일부), §11.4 (prepare) |
| 🔷 **P3** | monitoring / analytics / ops 자동화 | §10.6 (error_manager), §11.2 (v3), §13, §14 |
| ⏸ **Deferred** | Python 운영 스크립트 / RL / flatbuffer / 레거시 버전들 | §9.4 deprecated, §11.1, §11.7, §12, §15 |

---

## 사용 팁

- 각 섹션 상단에 **→ target** 을 명기. crate SPEC 작업 시 이 체크리스트의 해당 항목을 SPEC.md 의 "완료 조건" 으로 가져갈 것.
- `✅/🔶/🔷/⏸` 아이콘으로 phase 빠르게 필터.
- 누락 발견 시 이 파일에 추가 후 git diff 로 리뷰.
