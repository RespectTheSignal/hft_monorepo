# services/strategy — SPEC

## 역할
subscriber 에서 받은 `MarketEvent` 를 입력으로, `OrderRequest` 를 생성해 order-gateway 로 전달. **Phase 1 에서는 no-op strategy (eval 호출만 되고 주문은 0건) 를 골격으로**. 실제 알고리즘은 Phase 3+.

## 구조

```
 hot runtime
  ┌──────────────────────────────────────────────────┐
  │  strategy task                                   │
  │   loop {                                         │
  │     let (ev, stamps) = rx.recv().await;          │
  │     stamps.consumed_ms = clock.now_ms();         │
  │     hft_telemetry::record_stage_nanos(           │
  │         Stage::Consumed,                         │
  │         stamps.total_ns());                      │
  │     self.state.update(&ev);                      │
  │     if let Some(req) = self.eval(&ev) {          │
  │        order_tx.send(req)?;                      │
  │     }                                            │
  │   }                                              │
  └──────────────────────────────────────────────────┘
```

## 계약

- 입력 채널은 subscriber 의 `InprocQueue` (crossbeam).
- 출력 채널은 order-gateway 의 `InprocQueue` 또는 ZMQ PUSH (order-gateway 가 별도 프로세스인 경우).
- `eval(&MarketEvent) -> Option<OrderRequest>` 는 순수함수 지향. state 접근은 `&mut self.state` 로 통제.
- eval 안에서 alloc 최소화. `OrderRequest::client_id` 는 `Arc<str>` 이므로 pool 에서 pull.

## Phase 1 TODO

1. `NoopStrategy` — eval 이 `None` 만 리턴. 통합 테스트용.
2. `Strategy` trait (비공개) — `fn update`, `fn eval`. 여러 전략 플러그인 가능하게.
3. runtime 2개 + consume loop 골격.
4. `stamps.consumed_ms` 기록 + telemetry record.
5. 통합 테스트에서 E2E 지연 (exchange_server_ms → consumed_ms) 측정 가능한 상태.

## Anti-jitter 체크

- [ ] eval 함수 alloc 0
- [ ] 입력 채널 `try_recv` 는 non-block, `recv` (block) 는 단일 소비자면 OK
- [ ] state update 는 lock-free 또는 `parking_lot::Mutex` (async lock 금지)
- [ ] 주문 전송은 `try_send` — gateway 채널이 full 이면 warn + drop (Phase 1 에서는 실제 주문 0건이라 무관)

## 완료 조건 (Phase 1)

- [ ] NoopStrategy 가 10M 이벤트 소비하면서 stage 7 (consumed) p99.9 < 1ms
- [ ] E2E (ExchangeServer → Consumed) p99.9 < 50ms 은 *네트워크 제외* 상태에서 확인 (MockFeed + MockClock)
