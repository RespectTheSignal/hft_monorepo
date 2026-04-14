# Gate.io WS Ping-Pong Latency Test

WebSocket 프로토콜 레벨 ping/pong RTT를 측정하여 순수 네트워크 레이턴시 베이스라인을 제공하는 테스트.

## Architecture

```
  Local Machine                          Gate.io WS Gateway
  ┌──────────┐    WS Ping frame          ┌──────────┐
  │          │ ──────────────────────▶    │          │
  │ t_send   │                           │          │
  │          │    WS Pong frame           │          │
  │          │ ◀──────────────────────    │          │
  │ t_recv   │                           │          │
  └──────────┘                           └──────────┘

  Ping RTT = t_recv - t_send
  One-way  ≈ Ping RTT / 2
```

## Quick Start

```bash
python -m venv .venv && source .venv/bin/activate
pip install -r requirements.txt
python ping_pong_test.py
```

## Usage

```bash
# 기본 실행 (30 rounds, 0.5s interval)
python ping_pong_test.py

# 옵션 지정
python ping_pong_test.py --rounds 50 --interval 1.0
```

| Flag | Default | Description |
|------|---------|-------------|
| `--rounds` | `30` | Ping/pong 사이클 수 |
| `--interval` | `0.5` | 핑 간격 (초) |

## Metrics

| Metric | Description |
|--------|-------------|
| **Ping RTT** | WebSocket ping 전송 → pong 수신 왕복 시간 |
| **One-way estimate** | p50 RTT / 2 (단방향 네트워크 지연 추정) |

## Design Notes

- RFC 6455 WebSocket 프로토콜 레벨 ping/pong 사용 (애플리케이션 오버헤드 없음)
- 단일 연결에서 반복 측정 (커넥션 오버헤드 제외)
- BookTicker 네트워크 레이턴시, REST RTT와 비교하여 네트워크 베이스라인 검증

## API Reference

- WebSocket: `wss://fx-ws.gateio.ws/v4/ws/usdt`
- [Gate.io Futures WebSocket docs](../../../docs/gate/06_futures_websocket.md)
