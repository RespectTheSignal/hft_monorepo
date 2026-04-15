# hft-exchange-bybit — SPEC

## 역할
Bybit USDT perpetual WS 에서 BookTicker / Trade 수신.

## WS 엔드포인트
- Public: `wss://stream.bybit.com/v5/public/linear`
- 구독: `{"op":"subscribe","args":["orderbook.1.BTCUSDT","publicTrade.BTCUSDT"]}`
- REST 심볼: `GET /v5/market/instruments-info?category=linear`

## 주의
- `orderbook.1.<symbol>` = top-of-book only (book ticker 동등). depth 가 아니라 이게 맞는 채널.
- Bybit 은 heartbeat `{"op":"ping"}` 20s 주기 필요.
- response 의 `cts` (create time) 는 거래소 서버 시각 → `exchange_server_ms` / `server_time_ms`.

## Phase 1 TODO
- [ ] 스캐폴드 유지.

## Phase 2 TODO
1. WS 구독·파싱 구현.
2. subscription 은 심볼 200개 넘으면 분할.
3. ping 20s.
4. 자동 reconnect + resubscribe.

## Anti-jitter 체크
- 다른 vendor 와 동일 규약 (emit alloc-free, hot runtime).
- Bybit 은 메시지 burst 가 종종 있음 → parser 가 한 번에 여러 update 처리 가능해야 함 (WS frame 하나에 orderbook 다중 update).

## 완료 조건 (Phase 1)
- [ ] compile 만

## 완료 조건 (Phase 2)
- [ ] 200 symbol 스트리밍 p99.9 < 1ms (stage 3~4)
