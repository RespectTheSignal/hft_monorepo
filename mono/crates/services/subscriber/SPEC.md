# services/subscriber — SPEC

## 역할
publisher 의 PUB 을 SUB 으로 수신, wire bytes → `MarketEvent` + `LatencyStamps` 로 복원, downstream (strategy 또는 latency-probe) 에 전달.

## 구조

```
 hot runtime
  ┌────────────────────────────────────────────────────┐
  │  SubTask (tokio task, hot-pinned)                  │
  │   loop {                                           │
  │     let batch = sub.drain_batch(512, &mut out);    │
  │     for (topic, payload) in out {                  │
  │       stamps.subscribed_ms = clock.now_ms();       │
  │       let (ev, stamps) = decode(topic, payload);   │
  │       downstream.push(ev, stamps);                 │
  │     }                                              │
  │   }                                                │
  └────────────────────────────────────────────────────┘
```

downstream 은 세 종류 중 하나 (Phase 에 따라):
- `InprocQueue` — 같은 프로세스 안의 strategy task 로 전달 (권장)
- `InprocZmqPub` — 별도 프로세스의 strategy 용
- `DirectFn` — latency-probe 가 HDR histogram 에 바로 record

## emit 방식 결정

**Phase 1 결정: strategy 와 subscriber 를 같은 프로세스에 두고 `InprocQueue` (`crossbeam_channel::bounded(8192)`) 로 연결.**

이유:
- ZMQ 한 단 더 거치면 stage 가 늘어나 예산 압박
- strategy 가 하나의 subscriber 로부터만 소비하는 게 보통
- 필요 시 Phase 3+ 에서 inproc ZMQ 로 쉽게 전환 가능

## 계약

- SUB 는 topic prefix 구독. `hft_protocol::TopicBuilder` 와 동일한 prefix.
- drain_batch 의 out Vec 은 재사용 — subscriber struct 의 `scratch: Vec<(Vec<u8>, Vec<u8>)>` 필드.
  단, payload Vec<u8> 는 ZMQ 가 매 메시지마다 알로케이트함. Phase 2 에서 `zmq::Message` 를 직접 재사용하는 최적화 고려.
- decode 에러는 cold path: metric + drop.

## Phase 1 TODO

1. `main.rs` 골격 — runtime 2개, SubTask 1개, InprocQueue 생성 (strategy 스캐폴드와 연결).
2. SubTask 구현 — decode + stamps.subscribed_ms 기록.
3. `decode(topic, payload)` — topic 의 kind 문자열 (bookticker/trade/webbookticker) 로 분기.
4. downstream 이 채널인 경우 `try_send` + 실패 시 drop metric (strategy 가 느리면 drop 하되 증가 경고).
5. 통합 테스트 (`hft-testkit::pipeline_harness!`): Worker → Aggregator → SubTask → channel, 1만 건.

## Anti-jitter 체크

- [ ] SubTask 가 hot runtime 의 단일 worker 에 pin
- [ ] `drain_batch` 한 번에 너무 크게 가져오지 않음 (cap 512)
- [ ] decode 함수가 alloc 0
- [ ] crossbeam channel 은 `bounded`. `try_send` 로 WouldBlock 시 drop + metric
- [ ] `zmq::Message` 재사용 최적화는 Phase 2+ (Phase 1 은 성능 baseline 확보 후 필요하면)

## 완료 조건 (Phase 1)

- [ ] MockFeed → publisher → subscriber E2E 에서 stage 6·7 p99.9 < 1ms
- [ ] 10M 이벤트 처리 중 decode 에러 0, drop 0
- [ ] SIGTERM 에 SUB 종료 + channel drain + clean shutdown
