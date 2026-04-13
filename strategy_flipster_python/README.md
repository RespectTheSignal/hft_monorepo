# strategy_flipster_python

Flipster 선물 HFT 전략 시스템. Binance 등 외부 거래소 bookticker를 시그널로 사용하여 Flipster에서 매매.

## 아키텍처

```
Binance ZMQ PUB (tcp://...:6000)  ──┐
                                     ├──→  MarketDataAggregator  ──→  Strategy  ──→  OrderManager  ──→  Flipster REST
Flipster IPC (Unix socket)  ────────┘           ↑                       ↑
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
├── types.py                # 핵심 데이터 타입 (BookTicker, Order, Position 등)
├── error.py                # 에러 타입
├── market_data/
│   ├── feed.py             # MarketDataFeed Protocol
│   ├── binance_zmq.py      # Binance ZMQ SUB 클라이언트
│   ├── flipster_ipc.py     # Flipster IPC Unix socket 클라이언트
│   └── aggregator.py       # 멀티 피드 병합
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
# 설정 파일 준비
cp config.example.toml config.toml
# API 키 설정
export FLIPSTER_API_KEY="..."
export FLIPSTER_API_SECRET="..."

# 실행
PYTHONPATH=src python -m strategy_flipster.main config.toml
```

## 이벤트 루프

```
Task 1: market_data_loop
  └─ aggregator.recv() → strategy.on_book_ticker() → order_manager.submit()
  └─ timeout (tick_interval_ms) → strategy.on_timer() → order_manager.submit()

Task 2: user_data_ws_loop
  └─ Flipster WS → UserState 실시간 갱신

Task 3: shutdown_watcher
  └─ SIGINT/SIGTERM → graceful shutdown
```

- bookticker 수신 시 즉시 전략 실행
- bookticker 없으면 `tick_interval_ms` (기본 10ms) 간격으로 `on_timer` 호출

## Wire Format

### Binance BookTicker (120B, ZMQ 단일 프레임)

```
ZMQ frame: [1B type_len]["bookticker"][120B payload]
Payload: exchange(16) + symbol(32) + bid/ask/bid_sz/ask_sz(4×f64) + 5×i64(ms)
```

### Flipster BookTicker (104B, IPC 프레임)

```
IPC frame: [4B topic_len LE][topic][4B payload_len LE][104B payload]
Payload: symbol(32) + bid/ask/last/mark/index(5×f64) + 4×i64(ns)
```

### Binance Trade (128B)

```
Payload: exchange(16) + symbol(32) + price/size(2×f64) + id/create_time/create_time_ms(3×i64)
         + is_internal(1B) + padding(7B) + server_time/pub_sent/sub_recv/sub_dump(4×i64)
```

## 모듈별 테스트

```bash
# Binance ZMQ 피드 테스트
python scripts/test_binance_feed.py tcp://211.181.122.3:6000

# Flipster IPC 피드 테스트 (로컬 data_subscriber 필요)
python scripts/test_flipster_feed.py

# Flipster ZMQ 직접 연결 테스트
python scripts/test_flipster_zmq_feed.py tcp://211.181.122.104:7000

# 사용자 데이터 테스트 (REST + WS)
python scripts/test_user_data.py rest   # REST만
python scripts/test_user_data.py ws     # WS만
python scripts/test_user_data.py        # 둘 다
```

## 전략 작성법

`strategy/base.py`의 `Strategy` Protocol 구현:

```python
class MyStrategy:
    async def on_book_ticker(self, ticker: BookTicker, state: UserState) -> list[OrderRequest]:
        # 매 틱마다 호출 — 주문 요청 리스트 반환
        return []

    async def on_timer(self, state: UserState) -> list[OrderRequest]:
        # tick_interval_ms 간격 — bookticker 없어도 실행
        return []

    async def on_start(self, state: UserState) -> None: ...
    async def on_stop(self) -> None: ...
```

## 설계 원칙

- **Rust 번역 고려**: `@dataclass(frozen, slots)` → struct, `Protocol` → trait, `Enum` → enum
- **asyncio 기반**: uvloop, 모든 I/O 비동기
- **틱 + 타이머**: 매 bookticker 즉시 실행 + 인터벌 fallback
- **50~200개 심볼 동시 처리**
- **composition > inheritance**: 전역 상태 없음, magic method 없음

## 의존성

| Python | Rust 대응 |
|--------|-----------|
| pyzmq | zmq crate |
| websockets | tokio-tungstenite |
| httpx | reqwest |
| uvloop | tokio |
| structlog | tracing |
| tomli | toml |
