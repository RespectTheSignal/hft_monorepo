# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

<!-- team-reporter: enabled -->

## Project Overview

Flipster 선물 HFT 전략 시스템. Binance bookticker를 시그널로 사용하여 Flipster에서 매매.
50~200개 심볼 동시 처리, 틱 단위 전략 실행. 추후 Rust로 번역 예정.

## Monorepo Architecture

```
Binance ZMQ PUB (tcp://211.181.122.3:6000)  ──→  strategy (ZMQ SUB 직접)
Flipster WS → data_publisher (Rust, ZMQ PUB tcp://211.181.122.104:7000)
                    → data_subscriber (Rust, ZMQ SUB → IPC Unix socket)
                         → strategy_flipster_python (THIS PROJECT, IPC client)
                         → questdb_uploader (Rust, IPC → QuestDB)
```

## Wire Formats

### Binance BookTicker (120 bytes, ZMQ 단일 프레임)

프레이밍: `[1B type_len][type_str][payload]`
- type = "bookticker" → 120B payload
- type = "trade" → 128B payload

| Offset | Size | Type | Field |
|--------|------|------|-------|
| 0 | 16 | bytes | exchange (null-padded) |
| 16 | 32 | bytes | symbol (null-padded) |
| 48 | 8 | f64 LE | bid_price |
| 56 | 8 | f64 LE | ask_price |
| 64 | 8 | f64 LE | bid_size |
| 72 | 8 | f64 LE | ask_size |
| 80 | 8 | i64 LE | event_time (ms) |
| 88 | 8 | i64 LE | server_time (ms) |
| 96 | 8 | i64 LE | publisher_sent_ms |
| 104 | 8 | i64 LE | subscriber_received_ms |
| 112 | 8 | i64 LE | subscriber_dump_ms |

### Flipster BookTicker (104 bytes, IPC 프레임)

프레이밍: `[4B topic_len LE][topic][4B payload_len LE][payload]`

| Offset | Size | Type | Field |
|--------|------|------|-------|
| 0 | 32 | bytes | symbol (null-padded) |
| 32 | 8 | f64 LE | bid_price |
| 40 | 8 | f64 LE | ask_price |
| 48 | 8 | f64 LE | last_price |
| 56 | 8 | f64 LE | mark_price |
| 64 | 8 | f64 LE | index_price |
| 72 | 8 | i64 LE | server_ts_ns |
| 80 | 8 | i64 LE | publisher_recv_ns |
| 88 | 8 | i64 LE | publisher_sent_ns |
| 96 | 8 | i64 LE | subscriber_recv_ns |

## Flipster IPC Protocol

Connect to Unix socket at `/tmp/flipster_data_subscriber.sock` (configurable via `IPC_SOCKET_PATH`).

Registration: `REGISTER:<process_id>:<symbol1>,<symbol2>,...\n`
Topic format: `flipster_bookticker_{SYMBOL}`

## Code Style

- **Rust 번역 고려**: `@dataclass(frozen=True, slots=True)` → struct, `Protocol` → trait
- **asyncio 기반**: uvloop 사용
- **상속 금지**: composition only
- **전역 상태 금지**: 모든 상태는 명시적 전달
- **타입 힌트 필수**: 모든 함수에 type annotation

## Key Modules

- `types.py`: 모든 데이터 타입 + wire format 파서 (struct.Struct 프리컴파일)
- `market_data/`: Binance ZMQ + Flipster IPC → Aggregator
- `user_data/`: Flipster REST + WS private → UserState
- `execution/`: HMAC auth + REST 주문 + OrderManager
- `strategy/`: Strategy Protocol + 구현체

## Language

프로젝트 문서, 주석 모두 한국어.
