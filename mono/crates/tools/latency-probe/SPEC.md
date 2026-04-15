# tools/latency-probe — SPEC

## 역할
publisher 의 PUB 을 구독, 각 메시지의 `LatencyStamps` 를 읽어 stage 별 지연 histogram 출력.
**Phase 1 의 가장 중요한 툴** — 50ms 예산 달성 여부의 1차 판정 도구.

## 사용법 (CLI)

```
latency-probe \
  --connect tcp://publisher-host:5555 \
  --topics gate_bookticker_BTC_USDT,binance_bookticker_BTC_USDT \
  --duration 60s \
  --dump hdr.json
```

stdout 에 stage 별 p50 / p99 / p99.9 / max 출력. JSON 으로 dump 도.

## 구현

```rust
// main.rs (hot runtime 하나만)
let sub: SubSocket = ctx.sub(cfg.sub_endpoint, &topics, &cfg)?;
let clock = Arc::new(SystemClock);
let mut hist: HashMap<Stage, Histogram<u64>> = ...;

loop {
    let (_topic, payload) = sub.recv()?;
    let (_ev, stamps) = decode(&payload)?;
    let now = clock.now_ms();
    let now_ns = clock.now_nanos();

    for (stage, ms) in stamps.iter() {
        if ms == 0 { continue; }
        let delta_ms = now - ms;
        hist.entry(stage).or_default().record(delta_ms as u64 * 1_000_000)?;  // ns 단위 저장
    }

    if duration_elapsed { break; }
}

for (stage, h) in hist {
    println!("{:?}: p50={} p99={} p99.9={} max={}",
             stage, h.value_at_quantile(0.5), h.value_at_quantile(0.99),
             h.value_at_quantile(0.999), h.max());
}
```

## 계약
- 이 툴은 publisher/subscriber 를 건드리지 않고 **read-only** 로 관찰.
- `hft-protocol::decode_*` + `hft-time::Stage` 만 의존.
- 실행 시점에 사람이 보는 보고서 생성 + JSON dump.

## Phase 1 TODO

1. clap CLI.
2. SubSocket + loop + HDR record.
3. 출력 포맷 (텍스트 표 + JSON).
4. 추가 기능: `--live` 로 1초마다 rolling p99 출력.
5. 통합 테스트에서 이 툴을 assert 로 사용 가능한 library API 도 `pub fn` 으로 노출.

## 완료 조건
- [ ] 10만 샘플 수집 후 stage 별 정확한 p50/p99/p99.9 출력
- [ ] JSON dump 가 `hft-telemetry::dump_hdr` 의 포맷과 호환 (합쳐서 비교 가능)
