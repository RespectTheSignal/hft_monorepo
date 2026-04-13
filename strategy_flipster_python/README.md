# strategy_flipster_python

Flipster 선물 HFT 전략 시스템. 멀티 거래소 bookticker를 시그널로 사용하여 Flipster에서 매매.

## 아키텍처

```
Exchange ZMQ PUB ──────────────┐  binance(:6000), gate(:5559), bybit(:5558), bitget(:6010), okx(:6011)
                                ├──→  MarketDataAggregator  ──→  Strategy  ──→  OrderManager  ──→  Flipster REST
Flipster ZMQ PUB / IPC  ──────┘           ↑                       ↑
                                     BookTicker (정규화)      UserState
                                                                   ↑
                                                          Flipster WS (private)
                                                          + REST (초기 로딩)
```

## 디렉토리 구조

```
src/strategy_flipster/
├── main.py                 # 진입점, 이벤트 루프
├── config.py               # TOML 설정 로딩
├── types.py                # 핵심 데이터 타입 + wire format 파서
├── error.py                # 에러 타입
├── market_data/
│   ├── feed.py             # MarketDataFeed Protocol
│   ├── zmq_feed.py         # 범용 거래소 ZMQ 피드 (binance/gate/bybit/bitget/okx)
│   ├── flipster_zmq.py     # Flipster ZMQ 직접 연결 피드
│   ├── flipster_ipc.py     # Flipster IPC Unix socket 피드
│   ├── binance_zmq.py      # 하위호환 wrapper (→ zmq_feed.py)
│   └── aggregator.py       # 멀티 피드 → asyncio.Queue 병합
├── user_data/
│   ├── rest_client.py      # Flipster REST 계정 조회
│   ├── ws_client.py        # Flipster WS private 토픽
│   └── state.py            # UserState 관리
├── execution/
│   ├── auth.py             # HMAC-SHA256 서명
│   ├── rest_client.py      # 주문 실행 REST
│   └── order_manager.py    # 주문 라이프사이클
└── strategy/
    ├── base.py             # Strategy Protocol
    └── sample.py           # 샘플 전략
```

## 실행

```bash
cp config.example.toml config.toml
export FLIPSTER_API_KEY="..."
export FLIPSTER_API_SECRET="..."

PYTHONPATH=src python -m strategy_flipster.main config.toml
```

## 설정 (config.toml)

```toml
[flipster_feed]
mode = "zmq"                                    # "zmq" (원격) 또는 "ipc" (로컬)
zmq_address = "tcp://211.181.122.104:7000"

[[exchange_feeds]]
exchange = "binance"
zmq_address = "tcp://211.181.122.3:6000"

# [[exchange_feeds]]
# exchange = "gate"
# zmq_address = "tcp://...:5559"

[strategy]
tick_interval_ms = 10
```

## 이벤트 루프

```
Task 1: market_data_loop
  └─ aggregator.recv() → strategy.on_book_ticker() → order_manager.submit()
  └─ timeout (tick_interval_ms) → strategy.on_timer()

Task 2: user_data_ws_loop
  └─ Flipster WS → UserState 실시간 갱신

Task 3: shutdown_watcher
  └─ SIGINT/SIGTERM → graceful shutdown
```

## Wire Format

### 범용 거래소 BookTicker (120B, ZMQ 단일 프레임)

binance, gate, bybit, bitget, okx 공통 — data_publisher 동일 wire format

```
Frame: [1B type_len]["bookticker"][120B payload]
Payload: exchange(16) + symbol(32) + bid/ask/bid_sz/ask_sz(4×f64) + 5×i64(ms)
```

### Flipster BookTicker (104B, ZMQ multipart 또는 IPC)

```
ZMQ: multipart [topic_bytes][104B payload]
IPC: [4B topic_len LE][topic][4B payload_len LE][104B payload]
Payload: symbol(32) + bid/ask/last/mark/index(5×f64) + 4×i64(ns)
```

## 모듈별 테스트

```bash
# Flipster ZMQ 직접 연결 (실데이터 확인됨: 327 msg/s, 346 symbols)
python scripts/test_flipster_zmq_feed.py tcp://211.181.122.104:7000

# 범용 거래소 ZMQ (binance, gate 등)
python scripts/test_binance_feed.py tcp://211.181.122.3:6000

# Flipster IPC (로컬 data_subscriber 필요)
python scripts/test_flipster_feed.py

# 사용자 데이터 (REST + WS)
python scripts/test_user_data.py rest
python scripts/test_user_data.py ws
```

## 전략 작성법

```python
class MyStrategy:
    async def on_book_ticker(self, ticker: BookTicker, state: UserState) -> list[OrderRequest]:
        return []  # 매 틱마다 호출

    async def on_timer(self, state: UserState) -> list[OrderRequest]:
        return []  # tick_interval_ms 간격

    async def on_start(self, state: UserState) -> None: ...
    async def on_stop(self) -> None: ...
```

## 설계 원칙

- **Rust 번역 고려**: `@dataclass(frozen, slots)` → struct, `Protocol` → trait
- **asyncio + uvloop**: 모든 I/O 비동기
- **틱 + 타이머**: bookticker 즉시 실행 + 인터벌 fallback
- **50~200 심볼** / **멀티 거래소** 동시 처리
- **composition > inheritance**

## 의존성

| Python | Rust 대응 |
|--------|-----------|
| pyzmq | zmq crate |
| websockets | tokio-tungstenite |
| httpx | reqwest |
| uvloop | tokio |
| structlog | tracing |
| tomli | toml |
