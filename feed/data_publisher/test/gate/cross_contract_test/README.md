# Gate.io Cross-Contract BookTicker Latency Comparison

여러 Futures 컨트랙트의 `book_ticker` WebSocket 레이턴시를 비교하여, 거래량/유동성에 따른 레이턴시 차이를 분석하는 테스트.

## Architecture

```
  Clock Sync (Phase 1)          NTP-style offset estimation
                                (shared across all contracts)

  Measurement (Phase 2)         Sequential per contract:
  ┌─────────┐                   ┌─────────┐
  │ BTC_USDT│──▶ N samples      │ ETH_USDT│──▶ N samples  ...
  └─────────┘                   └─────────┘

  Comparison (Phase 3)          Side-by-side p50 table
```

## Quick Start

```bash
python -m venv .venv && source .venv/bin/activate
pip install -r requirements.txt
python cross_contract_test.py
```

## Usage

```bash
# 기본 실행 (BTC, ETH, SOL, DOGE, XRP — 각 30 samples)
python cross_contract_test.py

# 옵션 지정
python cross_contract_test.py --contracts BTC_USDT ETH_USDT SOL_USDT --samples 50
```

| Flag | Default | Description |
|------|---------|-------------|
| `--contracts` | `BTC_USDT ETH_USDT SOL_USDT DOGE_USDT XRP_USDT` | 비교 대상 컨트랙트 목록 |
| `--samples` | `30` | 컨트랙트당 수집할 메시지 수 |
| `--sync-rounds` | `10` | Clock sync 라운드 수 |

## Metrics

| Metric | Description |
|--------|-------------|
| **Total** | 매칭엔진 이벤트 → 로컬 수신 (end-to-end) |
| **Internal** | 매칭엔진 → WS 게이트웨이 (거래소 내부) |
| **Network** | WS 게이트웨이 → 로컬 머신 (네트워크 전송) |

## Design Notes

- 동일한 clock offset으로 모든 컨트랙트를 측정하여 공정한 비교 보장
- 순차 측정 (한 번에 하나의 컨트랙트) — 동시 구독시 메시지 경합 방지
- 거래량이 높은 컨트랙트 (BTC, ETH)가 내부 처리 지연이 다른지 확인 가능

## API Reference

- WebSocket: `wss://fx-ws.gateio.ws/v4/ws/usdt`
- REST: `https://api.gateio.ws/api/v4`
- Channel: `futures.book_ticker`
- [Gate.io Futures WebSocket docs](../../../docs/gate/06_futures_websocket.md)
