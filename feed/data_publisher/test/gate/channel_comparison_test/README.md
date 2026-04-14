# Gate.io WS Channel Comparison Latency Test

동일 컨트랙트에 대해 서로 다른 WebSocket 채널의 레이턴시를 비교하여, 어떤 데이터 소스가 가장 빠른지 분석하는 테스트.

## Channels Tested

| # | Channel | Description |
|---|---------|-------------|
| 1 | `futures.book_ticker` | BBO (Best Bid/Offer) 업데이트 |
| 2 | `futures.trades` | 개별 체결 이벤트 |
| 3 | `futures.order_book` | 오더북 diff 업데이트 (5 levels) |

## Quick Start

```bash
python -m venv .venv && source .venv/bin/activate
pip install -r requirements.txt
python channel_comparison_test.py
```

## Usage

```bash
# 기본 실행 (SOL_USDT, 3 channels, 30 samples each)
python channel_comparison_test.py

# 옵션 지정
python channel_comparison_test.py --contract BTC_USDT --samples 50 --timeout 60
```

| Flag | Default | Description |
|------|---------|-------------|
| `--contract` | `SOL_USDT` | 측정 대상 futures contract |
| `--samples` | `30` | 채널당 수집할 메시지 수 |
| `--sync-rounds` | `10` | Clock sync 라운드 수 |
| `--timeout` | `30` | 메시지 대기 타임아웃 (초) |

## Metrics

| Metric | Description |
|--------|-------------|
| **Total** | 이벤트 발생 → 로컬 수신 (end-to-end) |
| **Internal** | 이벤트 발생 → WS 게이트웨이 전송 (거래소 내부) |
| **Network** | WS 게이트웨이 → 로컬 수신 (네트워크 전송) |

## Design Notes

- 각 채널의 타임스탬프 추출 방식이 다름:
  - `book_ticker`: `result.t` (matching engine, ms)
  - `trades`: `result[0].create_time_ms` (체결 시각, seconds with ms decimal)
  - `order_book`: `result.t` (orderbook event, ms)
- 순차 측정으로 채널 간 메시지 경합 방지
- 타임아웃 설정으로 업데이트가 드문 채널에서 무한 대기 방지

## API Reference

- WebSocket: `wss://fx-ws.gateio.ws/v4/ws/usdt`
- [Gate.io Futures WebSocket docs](../../../docs/gate/06_futures_websocket.md)
