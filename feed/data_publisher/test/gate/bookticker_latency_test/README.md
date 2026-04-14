# Gate.io BookTicker Latency Test

Gate.io Futures `book_ticker` 채널의 WebSocket 데이터 전달 레이턴시를 측정하는 테스트 스위트.

## Architecture

```
                        Clock Sync (Phase 1)
                        ====================
  Local Machine                                    Gate.io REST API
  ┌──────────┐    GET /futures/usdt/tickers         ┌──────────┐
  │          │ ──────────────────────────────────▶   │          │
  │ local_t1 │                                      │ X-In-Time│ (μs)
  │          │   ◀────────────────────────────────── │X-Out-Time│ (μs)
  │ local_t2 │          200 OK + headers            │          │
  └──────────┘                                      └──────────┘

  offset = (X-In-Time + X-Out-Time)/2 - (local_t1 + local_t2)/2


                      Measurement (Phase 2)
                      =====================
  Matching Engine ──▶ WS Gateway ──────────────▶ Local Machine
       t (ms)         time_ms (ms)               local_recv (μs)
       │                  │                           │
       ├── Internal ──────┤                           │
       │              (time_ms - t)                   │
       │                  ├────── Network ────────────┤
       │                  │  (local_recv - time_ms    │
       │                  │      - offset)            │
       ├──────────────── Total ───────────────────────┤
       │         (local_recv - t - offset)            │
```

## Quick Start

```bash
python -m venv .venv && source .venv/bin/activate
pip install -r requirements.txt
python latency_test.py
```

## Usage

```bash
# 기본 실행 (SOL_USDT, 100 samples, 10 sync rounds)
python latency_test.py

# 옵션 지정
python latency_test.py --contract BTC_USDT --samples 200 --sync-rounds 20
```

| Flag | Default | Description |
|------|---------|-------------|
| `--contract` | `SOL_USDT` | 측정 대상 futures contract |
| `--samples` | `100` | 수집할 book_ticker 메시지 수 |
| `--sync-rounds` | `10` | NTP-style clock sync 라운드 수 |

## Output Example

```
[Phase 1] Clock Synchronization (10 rounds)...
  Offset: +2.009 ms  RTT: 38.778 ms  Stdev: 20.166 ms

[Phase 2] Collecting 50 book_ticker updates...
  Subscribed: subscribe
  [   1/50]  total=  17.32ms  internal=  2.00ms  network=  15.32ms  bid=86.13  ask=86.14
  ...

==========================================================================================
  RESULTS  (50 samples, contract=SOL_USDT)
==========================================================================================
         Total:  min=  15.28  p50=  16.98  p90=  33.10  p95=  34.21  p99=  39.15  max=  39.15  avg=  19.34  std=  6.24 ms
      Internal:  min=   0.00  p50=   1.00  p90=  16.00  p95=  18.00  p99=  22.00  max=  22.00  avg=   3.20  std=  5.74 ms
       Network:  min=  14.97  p50=  15.97  p90=  17.43  p95=  18.05  p99=  21.40  max=  21.40  avg=  16.14  std=  1.05 ms
==========================================================================================
  Clock offset : +2.009 ms  (stdev=20.166 ms, n=10)
  Network RTT  : 38.778 ms (median)
==========================================================================================
```

## Metrics

| Metric | Formula | Description |
|--------|---------|-------------|
| **Total** | `local_recv - t - offset` | 매칭엔진 이벤트 발생 → 로컬 수신까지 전체 지연 |
| **Internal** | `time_ms - t` | 매칭엔진 → WS 게이트웨이 (거래소 내부 처리) |
| **Network** | `local_recv - time_ms - offset` | WS 게이트웨이 → 로컬 머신 (네트워크 전송) |
| **Clock offset** | NTP-style median | 로컬-서버 시계 차이 (양수 = 서버가 빠름) |

## Clock Sync Method

Gate.io REST API 응답 헤더 `X-In-Time` / `X-Out-Time` (μs 정밀도)을 활용한 NTP-style 보정.

1. 로컬 송신 시각 `t1` 기록
2. REST API 호출
3. 로컬 수신 시각 `t2` 기록
4. 서버 수신 `X-In-Time`, 서버 송신 `X-Out-Time` 추출
5. `offset = (X-In + X-Out)/2 - (t1 + t2)/2`
6. N회 반복 후 median 사용 (outlier 제거)

## API Reference

- WebSocket: `wss://fx-ws.gateio.ws/v4/ws/usdt`
- REST: `https://api.gateio.ws/api/v4`
- Channel: `futures.book_ticker` — realtime best bid/ask (BBO)
- [Gate.io Futures WebSocket docs](../../../docs/gate/06_futures_websocket.md)
