# services/order-gateway — SPEC

## 역할
strategy 의 `OrderRequest` 를 받아 해당 거래소의 `ExchangeExecutor::place_order` 로 전송.
동시에 Go `order-processor` (browser automation 경로) 와 협업 — 일부 거래소 주문은 Go 쪽 경로로.

Phase 1 은 **골격만**. 실 거래소 Executor 구현은 Phase 4.

## 구조

```
 bg runtime (주문 경로는 hot 만큼 빡빡하지 않음 — 주문은 drop 불가, retry 책임)
  ┌──────────────────────────────────────────────────┐
  │  gateway task                                    │
  │   loop {                                         │
  │     let req = rx.recv().await;                   │
  │     let exec = self.executors[&req.exchange];    │
  │     match exec.place_order(req.clone()).await {  │
  │       Ok(ack) => ack_tx.send(ack),               │
  │       Err(e)  => self.retry_policy(req, e),      │
  │     }                                            │
  │   }                                              │
  └──────────────────────────────────────────────────┘

  Go order-processor 협업 경로:
     strategy → inproc ZMQ PUSH → order-gateway
     order-gateway → (routing) → rust exec OR go-ipc push → go/order-processor
```

## 계약

- **hot runtime 에 들어가지 않음**. 주문은 몇 ms ~ 수십 ms 걸려도 OK.
- idempotency: `req.client_id` 를 거래소에 그대로 넘겨 중복 전송 식별.
- retry policy: network error 는 exponential backoff (max 3회), rejected (예: insufficient balance) 는 즉시 포기 + error metric.
- ack 는 strategy 로 다시 보내거나 (optional) QuestDB 에 저장 (`orders` 테이블).

## Go order-processor 협업

- 기존 `gate_hft` 의 `order_processor_go/` 는 browser automation (일부 거래소 일부 기능에 browser 기반).
- Phase 1 에서는 routing table 만: `HashMap<(ExchangeId, Channel), Route>`. Route = `Rust(Arc<dyn ExchangeExecutor>)` 또는 `Go(ipc_endpoint)`.
- IPC 는 ZMQ REQ-REP (order-gateway REQ, go 서버 REP). latency 크리티컬 아님.

## Phase 1 TODO

1. `main.rs` 골격 — bg runtime, consume loop, executor map.
2. `NoopExecutor` (`place_order` 가 fake ack 리턴) — 테스트용.
3. Go IPC route 자리만 확보 (endpoint 문자열 받아 ZMQ REQ 소켓 생성).
4. retry policy struct.
5. ack channel 연결 (strategy 또는 storage).

## Anti-jitter 체크
- hot path 아님 → 비교적 관대.
- 그러나 strategy → gateway 채널이 full 이면 strategy 가 drop → 실주문 누락. channel capacity 넉넉히 (8192).
- gateway 의 blocking I/O 가 strategy 를 블로킹하지 않아야 함 (별도 runtime 이라 OK).

## 완료 조건 (Phase 1)

- [ ] NoopExecutor 로 strategy → gateway → ack 경로 E2E 테스트
- [ ] Go IPC endpoint 연결 실패 시 메시지 drop 없이 retry

## 완료 조건 (Phase 4)

- [ ] Binance / Gate / Bybit / Bitget / OKX 각 Executor 구현
- [ ] place_order → ack 까지 90%tile < 30ms (네트워크 포함)
