# services/publisher — SPEC

## 역할
거래소 feed → 직렬화 → worker PUSH → aggregator PULL → PUB. **Phase 1 의 하이라이트**.
50ms 예산의 stage 3·4·5·6 을 전부 담당.

## 구조 (프로세스 안)

```
 ┌──────────────────────────────────────────────────────────────────────┐
 │  hot runtime (cpuset: hot cores)                                     │
 │                                                                      │
 │   ┌───────────────┐   emit(event, stamps)    ┌─────────────────┐    │
 │   │ ExchangeFeed  │─────────────────────────►│  Worker N       │    │
 │   │  (tokio task) │   alloc-free 콜백        │  (tokio task)   │    │
 │   └───────────────┘                          │  serialize      │    │
 │                                              │  + PUSH dontwait│    │
 │                                              └────────┬────────┘    │
 │                                                       │ inproc ZMQ  │
 │                                                       ▼             │
 │                                              ┌─────────────────┐    │
 │                                              │  Aggregator     │    │
 │                                              │  (tokio task)   │    │
 │                                              │  drain_batch    │    │
 │                                              │  + PUB dontwait │    │
 │                                              └────────┬────────┘    │
 └───────────────────────────────────────────────────────┼──────────────┘
                                                         │ tcp ZMQ PUB
                                                         ▼
                                               (subscribers, storage)

 ┌─────────────────────────────────────────┐
 │  bg runtime                             │
 │   - hft-telemetry OTel exporter         │
 │   - prometheus http server              │
 │   - config supabase refresh (startup)   │
 │   - health probe http server            │
 └─────────────────────────────────────────┘
```

## 핵심 수치

- Worker 수 = 거래소 feed 수 × 1 (1 feed 당 1 worker). Gate 의 경우 Primary + Secondary 로 2.
- Aggregator = 1 task (PUB 은 single-thread).
- drain batch cap = 512 (config).
- ZMQ HWM = 100_000 (PUSH, PUB 모두).

## 경로별 책임

| stage | 어디서 | 코드 포인트 |
|---|---|---|
| `exchange_server_ms` | exchange crate emit 시 | `hft-exchange-<v>::stream` |
| `ws_received_ms` | 동상 | 동상 |
| `serialized_ms` | worker 의 `on_event` | `hft-protocol::encode_*` 직후 |
| `pushed_ms` | worker 의 `on_event` | `PushSocket::send_dontwait` 직후 |
| `published_ms` | aggregator drain loop | `PubSocket::send_dontwait` 직후 |

각 stage 에 진입하기 직전의 `clock.now_ms()` 를 `stamps` 에 set, 바이트로 같이 실어 PUB.

## emit 콜백 구조 (worker 안에서)

```rust
struct Worker {
    clock: Arc<dyn Clock>,
    buf: [u8; max(BOOK_TICKER_SIZE, TRADE_SIZE)],   // stack-sized
    push: PushSocket,                                // hft-zmq
    topics: Arc<TopicBuilder>,
}

impl Worker {
    fn on_event(&mut self, event: MarketEvent, mut stamps: LatencyStamps) {
        stamps.serialized_ms = self.clock.now_ms();
        let (topic, size) = match event {
            MarketEvent::BookTicker(bt)    => (self.topics.get(bt.ex, MSG_BOOKTICKER, &bt.symbol),
                                               hft_protocol::encode_bookticker(&bt, &stamps, &mut self.buf)),
            MarketEvent::WebBookTicker(bt) => (self.topics.get(bt.ex, MSG_WEBBOOKTICKER, &bt.symbol),
                                               hft_protocol::encode_bookticker(&bt, &stamps, &mut self.buf)),
            MarketEvent::Trade(tr)         => (self.topics.get(tr.ex, MSG_TRADE, &tr.symbol),
                                               hft_protocol::encode_trade(&tr, &stamps, &mut self.buf)),
        };
        stamps.pushed_ms = self.clock.now_ms();
        // stamps 는 이미 buf 에 박혀있음. send 만.
        match self.push.send_dontwait(topic, &self.buf[..size]) {
            SendOutcome::Sent       => pipeline_event_inc("pushed"),
            SendOutcome::WouldBlock => zmq_dropped_inc("push"),
            SendOutcome::Error(e)   => { /* cold path */ }
        }
    }
}
```

주의: **`stamps.pushed_ms` 를 encode 이후에 set 하려면 wire 에 stamps 가 들어간 후 buf 위에 덮어써야** 함. 두 가지 선택:
- (A) encode 2회 (pushed 직전 다시 encode) — alloc 은 없지만 CPU 중복
- (B) encode 후 stamps 영역의 오프셋에 직접 write (`hft-protocol::patch_stamps(&mut buf, Stage::Pushed, ms)`)

**결정: (B) 채택.** `hft-protocol` 에 `patch_stamp` 함수 추가 (Phase 1 TODO 에 넣기).

## Startup 순서

1. `hft_config::load_all()?`
2. `hft_telemetry::init(&cfg.telemetry, "publisher-<id>")?`
3. runtime 2개 (hot/bg) 생성. hot workers = `cfg.hot_workers` (보통 거래소 수 + 2)
4. `hft_zmq::Context::new()`
5. Aggregator task spawn (PULL inproc, PUB tcp)
6. 각 거래소 `ExchangeConfig` 마다 `load_feed(cfg)?` → `Box<dyn ExchangeFeed>` + Worker
7. 각 Worker 를 hot runtime 에 spawn. Feed 의 `stream(symbols, emit, cancel)` 호출
8. Warm-up (옵션): `hft_testkit::WarmUp` 로 1만 건 dummy emit → aggregator 가 소비했는지 확인
9. readiness probe 200 → 외부 subscriber 구독 가능 상태 표시

## Phase 1 TODO

1. `main.rs` 에 runtime 2개 분리 골격 (위 코드 스케치대로).
2. `Worker` struct + `on_event` 구현 — `hft-protocol::patch_stamp` 의존 (=> `hft-protocol` SPEC 에도 TODO 추가 완료).
3. `Aggregator` struct — drain_batch(512) → PUB send_dontwait loop. PUB topic 은 multipart 1st frame.
4. `load_feed(cfg) -> Box<dyn ExchangeFeed>` — Phase 1 은 `MockFeed` 리턴 (from `hft-testkit`). Phase 2 에서 vendor 선택.
5. readiness http probe (axum bg).
6. graceful shutdown: SIGTERM → `cancel.cancel()` → Feed 종료 → Aggregator drain remaining → flush telemetry.
7. 통합 테스트: MockFeed → Worker → Aggregator → MockSub 로 1만 건, p99.9 stage 3~6 < 2ms.

## Anti-jitter 체크 (중요)

- [ ] `on_event` 안에서 allocation 0 (dhat)
- [ ] Aggregator drain 이 cap 을 초과하지 않아 다른 task 를 굶기지 않음
- [ ] Worker 와 Aggregator 가 다른 tokio worker thread 에서 돌게 pin (taskset hint via hft-telemetry)
- [ ] PUSH / PUB 모두 `DONTWAIT`
- [ ] `patch_stamp` 는 `u64::to_le_bytes` + 오프셋 write — syscall 없음
- [ ] `info!` 는 startup / shutdown 에만

## 완료 조건 (Phase 1)

- [ ] MockFeed 로 100K events/s 주입 시 p99.9 stage 3~6 < 2ms, drop 0
- [ ] Worker alloc 0, Aggregator alloc 0
- [ ] readiness probe green 후에만 PUB socket bind 완료
- [ ] SIGTERM 에 5s 안에 clean shutdown
