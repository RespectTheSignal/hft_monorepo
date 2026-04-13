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

## 설정

`config.example.toml`을 복사하여 `config.toml` 생성:

```bash
cp config.example.toml config.toml
```

API 키는 환경변수 사용:
```bash
export FLIPSTER_API_KEY="..."
export FLIPSTER_API_SECRET="..."
```

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

## 테스트 스크립트

```bash
# Binance ZMQ 피드 테스트
python scripts/test_binance_feed.py tcp://211.181.122.3:6000

# Flipster IPC 피드 테스트 (로컬 data_subscriber 필요)
python scripts/test_flipster_feed.py

# Flipster ZMQ 직접 연결 테스트
python scripts/test_flipster_zmq_feed.py tcp://211.181.122.104:7000
```

## 설계 원칙

- **Rust 번역 고려**: dataclass(frozen) → struct, Protocol → trait, enum → enum
- **asyncio 기반**: uvloop 사용, 모든 I/O 비동기
- **틱 단위 처리**: 매 bookticker + 타이머 인터벌(기본 10ms)로 전략 실행
- **50~200개 심볼 동시 처리** 대응
