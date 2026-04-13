# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

<!-- team-reporter: enabled -->

## Project Overview

Python trading strategy for Flipster exchange. This is part of `hft_monorepo` — a market data pipeline + strategy system for Flipster perpetual futures.

## Monorepo Architecture

This strategy consumes real-time bookticker data from a Rust feed pipeline:

```
Flipster WS → data_publisher (Rust, ZMQ PUB tcp://:7000)
                    → data_subscriber (Rust, ZMQ SUB → IPC Unix socket)
                         → strategy_flipster_python (THIS PROJECT, IPC client)
                         → questdb_uploader (Rust, IPC → QuestDB)
```

### IPC Protocol (how this strategy receives data)

Connect to Unix socket at `/tmp/flipster_data_subscriber.sock` (configurable via `IPC_SOCKET_PATH`).

**Registration** — send as first line after connect:
```
REGISTER:<process_id>:<symbol1>,<symbol2>,...\n
```
Empty symbol list = subscribe all. Example: `REGISTER:strategy:BTC-USDT-PERP,ETH-USDT-PERP\n`

**Message framing** (pushed by subscriber after registration):
```
[4B topic_len LE][topic bytes][4B payload_len LE][payload bytes]
```

**Payload: FlipsterBookTicker (104 bytes, little-endian C-struct)**:

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

Topic format: `flipster_bookticker_{SYMBOL}` (e.g., `flipster_bookticker_BTC-USDT-PERP`)

## Language

Project documentation and comments are in Korean. Follow that convention.

<!-- team-reporter: enabled -->