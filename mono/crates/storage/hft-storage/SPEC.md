# hft-storage — SPEC

## 역할
QuestDB 로 시장 데이터 + latency 샘플 저장. **절대 hot path 에서 직접 부르지 않음**. ZMQ SUB 으로 독립 수신.

## 공개 API

```rust
pub struct QuestDbSink {
    sender: questdb::ingress::Sender,
    batch_rows: usize,
    batch_ms: u64,
    spool_dir: PathBuf,
    buffer: questdb::ingress::Buffer,
    last_flush: Instant,
}

impl QuestDbSink {
    pub fn new(cfg: &QuestDbConfig) -> anyhow::Result<Self>;
    pub fn push_bookticker(&mut self, bt: &BookTicker, stamps: &LatencyStamps) -> anyhow::Result<()>;
    pub fn push_trade(&mut self, tr: &Trade, stamps: &LatencyStamps) -> anyhow::Result<()>;
    pub fn push_latency_sample(&mut self, stage: Stage, nanos: u64) -> anyhow::Result<()>;
    pub fn maybe_flush(&mut self) -> anyhow::Result<()>;  // batch_rows/batch_ms 넘으면 flush
    pub fn force_flush(&mut self) -> anyhow::Result<()>;
}
```

## 테이블 스키마

- `bookticker(symbol SYMBOL, bid_price DOUBLE, ask_price DOUBLE, bid_size DOUBLE, ask_size DOUBLE, event_time_ms LONG, server_time_ms LONG, ts TIMESTAMP)` partition DAY
- `trade(...)` 동일 구조
- `latency_stages(stage SYMBOL, nanos LONG, ts TIMESTAMP)` partition HOUR

startup 에 `CREATE TABLE IF NOT EXISTS` 실행 (ILP 가 아니라 PG wire 로, rarely-called path).

## 계약
- 이 crate 는 **SUB 서비스에서만** 사용. publisher/subscriber/strategy 는 건드리지 않음.
- Sink 메서드는 batch 안 flush 이므로 빠름 (< 1μs). `maybe_flush` 는 주기적 bg task 에서 호출.
- 연결 장애 시 `spool_dir` 에 `.ilp` 파일로 쌓고 복구 시 replay. hot path 영향 없어야 하는 이유.

## Phase 1 TODO

1. `questdb-rs` crate 의존성 + ILP 연결.
2. 스키마 init 루틴 (PG wire over 8812) — postgres crate.
3. batch buffer + `maybe_flush` 로직.
4. 연결 retry (exponential backoff 100ms → 30s cap).
5. spool-on-error: `questdb-rs` 의 `Buffer` 를 파일로 serialize (bincode 또는 ILP text).
6. `storage-svc` 용 바이너리는 `tools/questdb-export` 에 (tools 쪽 SPEC 참조).
7. 테스트: local docker QuestDB 띄우고 100K rows/s flush 실패율 0.

## Anti-jitter 체크
- `push_*` 는 Buffer 에 append 만 (no alloc 시 buffer 가 pre-sized).
- `maybe_flush` 호출 주기는 bg task 의 100ms tick — hot 에선 절대 안 부름.
- spool file 쓰기는 bg thread. `fsync` 없이 `write` + 별도 주기적 `fsync`.

## 완료 조건
- [ ] 100K rows/s 30분 지속 flush 실패 0
- [ ] 연결 끊기면 spool 에 쌓이고 복구 시 유실 없이 replay
- [ ] publisher hot p99.9 에 이 crate 가 영향 0 (다른 프로세스로 돌리면 trivially 만족)
