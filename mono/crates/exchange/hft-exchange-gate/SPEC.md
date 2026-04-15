# hft-exchange-gate — SPEC

## 역할
Gate.io 의 **이중 WS** (API + Web) 로 BookTicker/Trade 수신. Gate 는 `DataRole::Primary` (API) 와 `DataRole::Secondary` (Web) 둘 다를 따로 지원. `MarketEvent::WebBookTicker` 는 secondary 에서만 내보냄.

## 공개 API

```rust
pub struct GateFeed {
    cfg: GateConfig,
    role: DataRole,               // Primary = API WS, Secondary = Web WS
    clock: Arc<dyn Clock>,
}

#[async_trait]
impl ExchangeFeed for GateFeed { /* role 에 따라 분기 */ }
```

## WS 엔드포인트
- API (Primary): `wss://fx-ws.gateio.ws/v4/ws/usdt` — channel `futures.book_ticker`, `futures.trades`
- Web (Secondary): `wss://fx-ws-web.gateio.ws/...` (기존 gate_hft 의 endpoint 재확인 필요)
  - 이쪽 BookTicker 는 `MarketEvent::WebBookTicker` 로 emit

## 계약
- 같은 프로세스 안에 `GateFeed { role: Primary }` 와 `GateFeed { role: Secondary }` 를 **두 인스턴스** 만들어 각각 `stream()`.
- `publisher` 는 두 Feed 를 모두 구독, topic 에 role 을 포함시켜 downstream 에서 구분.
- WebBookTicker 는 downstream 이 비교/분석용으로만 사용. strategy 가 주 신호로 쓰지 않음 (Phase 3+ 에서 결정).

## Phase 1 TODO
- [ ] 스캐폴드 유지. `role` 필드 추가.
- [ ] SPEC 에 이중 WS 규약 명시 (이 파일 자체).

## Phase 2 TODO
1. Primary WS (API) 구현 — book_ticker / trades 채널 구독, JSON 파싱.
2. Secondary WS (Web) 구현 — 엔드포인트/페이로드 차이는 기존 gate_hft 의 `web_bookticker_publisher.rs` 참조.
3. 두 WS 가 서로 독립적으로 reconnect.
4. ping/pong — Gate 는 서버 ping 10s 간격.
5. 심볼 리스트: REST `GET /api/v4/futures/usdt/contracts`.

## Anti-jitter 체크
- 두 WS 가 같은 hot runtime worker 에서 poll 될 수 있음. 문제는 없으나 worker 수를 넉넉히 (Feed 수 × 2).
- WebBookTicker 의 emit 도 Primary 와 같은 alloc-free 규약.

## 완료 조건 (Phase 1)
- [ ] `GateFeed::new(cfg, role)` compile
- [ ] role 을 config 에서 받아 두 인스턴스 만드는 샘플 코드가 `configs/publisher-gate.toml` 에 있음

## 완료 조건 (Phase 2)
- [ ] Primary + Secondary 동시 스트리밍 24h drop 0
- [ ] WebBookTicker 가 Primary BookTicker 와 분리돼 QuestDB 에 기록
