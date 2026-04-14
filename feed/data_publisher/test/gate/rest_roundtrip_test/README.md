# Gate.io REST API Round-Trip Latency Test

Gate.io Futures REST API의 주요 엔드포인트별 Round-Trip Time(RTT)을 측정하고, 서버 처리 시간과 네트워크 지연을 분리하는 테스트 스위트.

## Architecture

```
  Local Machine                                        Gate.io REST API
  ┌──────────┐    GET /futures/usdt/{endpoint}          ┌──────────┐
  │          │ ──────────────────────────────────────▶   │          │
  │ local_t1 │                                          │ X-In-Time│ (μs)
  │          │                                          │ (server  │
  │          │                                          │  proc)   │
  │          │   ◀────────────────────────────────────── │X-Out-Time│ (μs)
  │ local_t2 │          200 OK + headers                │          │
  └──────────┘                                          └──────────┘

  Total RTT   = local_t2 - local_t1
  Server proc = X-Out-Time - X-In-Time
  Network RTT = Total RTT - Server proc
```

## Quick Start

```bash
python -m venv .venv && source .venv/bin/activate
pip install -r requirements.txt
python roundtrip_test.py
```

## Usage

```bash
# 기본 실행 (SOL_USDT, 30 rounds, 3 warmup)
python roundtrip_test.py

# 옵션 지정
python roundtrip_test.py --contract BTC_USDT --rounds 50 --warmup 5
```

| Flag | Default | Description |
|------|---------|-------------|
| `--contract` | `SOL_USDT` | 측정 대상 futures contract |
| `--rounds` | `30` | 엔드포인트당 요청 횟수 |
| `--warmup` | `3` | Warmup 요청 횟수 (TCP/TLS 커넥션 프라이밍) |

## Endpoints Tested

| # | Endpoint | Path | Description |
|---|----------|------|-------------|
| 1 | `tickers` | `/futures/usdt/tickers?contract=X` | 단일 티커 (경량) |
| 2 | `order_book` | `/futures/usdt/order_book?contract=X&limit=5` | 오더북 스냅샷 (5 depth) |
| 3 | `trades` | `/futures/usdt/trades?contract=X&limit=1` | 최근 체결 (1건) |
| 4 | `contract_info` | `/futures/usdt/contracts/X` | 컨트랙트 상세 정보 |

## Output Example

```
==========================================================================================
  Gate.io Futures REST API Round-Trip Latency Test
  Contract  : SOL_USDT
  Rounds    : 20 (+3 warmup)
  Endpoints : tickers, order_book, trades, contract_info
  Base URL  : https://api.gateio.ws/api/v4
==========================================================================================

  Benchmarking: tickers  (/futures/usdt/tickers)
    [   1/20]  total=  41.46ms  server=  1.60ms  network=  39.87ms
    ...

  [tickers] (20 samples)
       Total RTT:  min=  39.81  p50=  40.37  p90=  41.46  p95=  41.61  ...
     Server proc:  min=   0.70  p50=   0.92  p90=   1.47  p95=   1.60  ...
     Network RTT:  min=  39.02  p50=  39.43  p90=  39.97  p95=  40.27  ...

==========================================================================================
  COMPARISON (p50 values in ms)
==========================================================================================
         Endpoint     Total    Server   Network   Samples
  ---------------  --------  --------  --------  --------
          tickers     40.37      0.92     39.43        20
       order_book     40.48      1.17     39.21        20
           trades     40.86      0.98     39.35        20
    contract_info     40.12      0.81     39.25        20
==========================================================================================
```

## Metrics

| Metric | Formula | Description |
|--------|---------|-------------|
| **Total RTT** | `local_recv - local_send` | 요청 전송 → 응답 수신까지 전체 왕복 시간 |
| **Server proc** | `X-Out-Time - X-In-Time` | 서버 내부 처리 시간 (μs 정밀도) |
| **Network RTT** | `Total RTT - Server proc` | 양방향 네트워크 전송 시간 |

## Design Notes

- **Warmup**: 첫 N회 요청은 TCP/TLS 핸드셰이크 오버헤드를 포함하므로 측정에서 제외
- **Rate limiting**: 요청 간 50ms 간격으로 429 (Too Many Requests) 방지
- **X-In-Time / X-Out-Time**: Gate.io 응답 헤더에 포함된 서버측 타임스탬프 (μs 단위)

## API Reference

- REST Base URL: `https://api.gateio.ws/api/v4`
- [Gate.io Futures REST API docs](../../../docs/gate/05_futures_rest_api.md)
