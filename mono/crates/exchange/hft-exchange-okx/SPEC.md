# hft-exchange-okx — SPEC

## 역할
OKX USDT swap WS 에서 BookTicker / Trade 수신.

## WS 엔드포인트
- Public: `wss://ws.okx.com:8443/ws/v5/public`
- 구독: `{"op":"subscribe","args":[{"channel":"bbo-tbt","instId":"BTC-USDT-SWAP"},{"channel":"trades","instId":"BTC-USDT-SWAP"}]}`
- `bbo-tbt` = tick-by-tick best bid/ask. 일반 `bbo` 보다 빠름.
- REST: `GET /api/v5/public/instruments?instType=SWAP`

## 주의
- OKX 심볼은 `-` 구분 (`BTC-USDT-SWAP`) — internal `Symbol` 로 저장 시 변환 규약 결정 필요 (그대로 저장 vs `BTC_USDT` 로 정규화).
  → 결정: **거래소별 raw 심볼 유지, downstream 비교 시 정규화 매퍼 사용.** 매퍼는 `hft-types::SymbolMap` 유틸 (Phase 2 에 추가).
- OKX 는 `pong` 자동 응답 (text `ping` 보내면 `pong` 받음). 28s 이내 아무 traffic 없으면 끊음.
- `ts` 는 ms epoch 거래소 시각.

## Phase 1 TODO
- [ ] 스캐폴드 유지.
- [ ] `hft-types` 의 SymbolMap 필요성 Phase 2 TODO 에 기록.

## Phase 2 TODO
1. WS 구독·파싱 구현.
2. 심볼 정규화 매퍼 (OKX `BTC-USDT-SWAP` ↔ 공통 `BTC_USDT`).
3. ping 25s 주기.
4. reconnect.

## Anti-jitter 체크
- `bbo-tbt` 는 메시지 양 많음. parser 효율 중요.
- 다른 vendor 와 동일 규약.

## 완료 조건 (Phase 1)
- [ ] compile 만

## 완료 조건 (Phase 2)
- [ ] 200 symbol 스트리밍 p99.9 < 1ms (stage 3~4)
- [ ] 심볼 매퍼 라운드 트립 테스트 통과
