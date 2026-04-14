# Gate.io WS First-Message Latency Test

WebSocket 연결 개시부터 첫 번째 데이터 메시지 수신까지의 레이턴시를 측정하는 테스트. HFT 시스템의 재접속 성능 평가에 활용.

## Architecture

```
  connect start ──▶ TCP+TLS+WS Handshake ──▶ connected
                    ├── Connect ──────────┤
                                             send subscribe ──▶ receive ack
                                             ├── Subscribe ────┤
                                                                ack ──▶ first update
                                                                ├── First msg ──┤
  ├──────────────────────── Total ──────────────────────────────────────────────┤
```

## Quick Start

```bash
python -m venv .venv && source .venv/bin/activate
pip install -r requirements.txt
python first_message_test.py
```

## Usage

```bash
# 기본 실행 (SOL_USDT, 10 rounds)
python first_message_test.py

# 옵션 지정
python first_message_test.py --contract BTC_USDT --rounds 20
```

| Flag | Default | Description |
|------|---------|-------------|
| `--contract` | `SOL_USDT` | 측정 대상 futures contract |
| `--rounds` | `10` | Connect/disconnect 사이클 수 |

## Metrics

| Metric | Description |
|--------|-------------|
| **Total** | 연결 시작 → 첫 update 수신까지 전체 시간 |
| **Connect** | TCP + TLS + WebSocket 핸드셰이크 |
| **Subscribe** | subscribe 전송 → ack 수신 |
| **First update** | ack 수신 → 첫 book_ticker update 수신 |

## Design Notes

- 매 라운드마다 연결을 완전히 끊고 새로 생성 (cold start 측정)
- 라운드 간 0.5초 대기로 서버측 rate limit 방지
- `futures.book_ticker` 채널 사용 (가장 빈번한 업데이트)

## API Reference

- WebSocket: `wss://fx-ws.gateio.ws/v4/ws/usdt`
- Channel: `futures.book_ticker`
- [Gate.io Futures WebSocket docs](../../../docs/gate/06_futures_websocket.md)
