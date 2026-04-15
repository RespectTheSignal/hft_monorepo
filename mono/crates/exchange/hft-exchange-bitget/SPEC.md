# hft-exchange-bitget — SPEC

## 역할
Bitget USDT-M perpetual WS 에서 BookTicker / Trade 수신.

## WS 엔드포인트
- Public: `wss://ws.bitget.com/v2/ws/public`
- 구독: `{"op":"subscribe","args":[{"instType":"USDT-FUTURES","channel":"books1","instId":"BTCUSDT"}]}`
- `books1` = top-of-book. `trade` = 개별 체결.
- REST 심볼: `GET /api/v2/mix/market/contracts?productType=usdt-futures`

## 주의
- Bitget 은 서버가 30s 간격으로 ping 보내지 않으면 끊음 → 클라가 30s 마다 `ping` 프레임 송신 (그냥 text "ping", 응답 "pong").
- push frame 의 `ts` 가 거래소 시각.

## Phase 1 TODO
- [ ] 스캐폴드 유지.

## Phase 2 TODO
1. WS 구독·파싱 구현.
2. ping text 30s 주기.
3. reconnect + resubscribe.
4. `books1` / `trade` 채널 분리 가능성 고려 (같은 WS 에 섞임).

## Anti-jitter 체크
- Bitget JSON 이 상대적으로 heavy — sonic-rs 또는 simd-json 사용 권장.
- 다른 vendor 와 동일 규약.

## 완료 조건 (Phase 1)
- [ ] compile 만

## 완료 조건 (Phase 2)
- [ ] 24h 스트리밍 reconnect ≤ 5회, drop 0
