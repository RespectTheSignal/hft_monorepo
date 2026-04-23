# CLAUDE.md

## Project Overview

FX bookticker 수집기. Binance / dxFeed / Databento 세 소스를 지원하고, Gate FX 심볼 포맷으로 정규화하여 QuestDB `fx_bookticker` 테이블에 적재.

**용도**: Gate HFT 전략에서 reference exchange를 FX로 확장하기 전 탐색용. "먹을거 있는지 눈으로 판단".

## 동작 방식

각 소스는 `FxSource` Protocol 구현체. `main.py`가 `--source {binance|dxfeed|databento}`에 따라 하나를 띄우고 → `QuestDbSink`로 직접 쓴다. 멀티소스 병렬 실행은 프로세스를 여러개 띄우는 방식.

`source_ts`(exchange event time)와 `recv_ns`(로컬 수신 monotonic)를 둘 다 기록해서 `delay_ms = (recv_ns - source_ns)/1e6`로 latency를 넣는다 — 소스별 latency 비교가 1차 목적.

## Key Modules

- `types.py`: `FxTick` (frozen dataclass, slots)
- `questdb_sink.py`: `questdb` ILP SDK 래퍼, `flush_interval_ms` 주기 flush
- `symbol_map.py`: config TOML에서 `[symbols.<source>]` 읽어 raw → gate symbol
- `sources/base.py`: `FxSource` Protocol (`async def run(sink)`)
- `sources/binance.py`: `wss://stream.binance.com:9443/stream?streams=eurusdt@bookTicker/...`
- `sources/dxfeed.py`: DXLink WSS, `SETUP → AUTH → CHANNEL_REQUEST(FEED) → FEED_SUBSCRIPTION`
- `sources/databento.py`: `databento.Live` 클라이언트, dataset/schema는 config
- `main.py`: CLI, config 로드, 소스 부팅

## Code Style

- 프로젝트 전반 한국어 주석/문서
- `@dataclass(frozen=True, slots=True)` + Protocol (상속 금지)
- 전역 상태 금지
- 타입 힌트 필수
- asyncio + uvloop (가능 시)

## Delay 측정 규약

`delay_ms = (time.time_ns() - source_ns_at_recv) / 1e6`

- `source_ns_at_recv`: 각 소스 메시지에서 뽑은 exchange event time을 ns로 환산
  - Binance `E` (ms) → `*1_000_000`
  - dxFeed `eventTime` (ms, UTC epoch) → `*1_000_000`
  - Databento `ts_event` (ns) → 그대로
- 로컬 수신 시점: `time.time_ns()` (wall clock — NTP 동기화된 서버 기준)
- 음수 나올 수 있음(clock skew). 그냥 기록.

## 의존성 (optional-extras)

- `dxfeed`: 기본 `websockets`로 충분 (DXLink는 순수 WS JSON)
- `databento`: `databento` Python SDK 필요 → `uv sync --extra databento`

## 확장 포인트

Latency 제일 낮은 소스 확정되면 → `feed/data_publisher` wire format(120B, ZMQ PUB)으로 래핑해서 strategy가 그대로 subscribe하게 만든다.
