# hft-telemetry — SPEC

## 역할
tracing + OTel + HDR histogram + prometheus 노출. **hot path 가 로깅으로 인해 튀지 않게** 하는 것이 1차 목표.

## 공개 API

```rust
pub fn init(cfg: &TelemetryConfig, service_name: &str) -> anyhow::Result<TelemetryHandle>;

pub struct TelemetryHandle { /* drop 시 flush */ }

pub fn record_stage_nanos(stage: Stage, nanos: u64);   // lock-free HDR append
pub fn dump_hdr() -> HashMap<Stage, HdrSnapshot>;       // tools/latency-probe 에서 호출

pub fn next_hot_core() -> Option<usize>;                // taskset hint, linux only
pub fn pin_current_thread(core: usize);                 // linux only, macos no-op

// prometheus counters/gauges (macro-free API)
pub fn zmq_dropped_inc(stage: &'static str);
pub fn pipeline_event_inc(stage: &'static str);
```

## 설계

### Subscriber 구성
```
tracing::Subscriber 구성:
  - fmt::Layer  — non_blocking stdout (async writer + MPSC bounded 4096)
                  JSON format in prod, pretty in dev
  - OtelLayer   — tracer provider: otlp over grpc, batch span processor
                  endpoint from TelemetryConfig.otlp_endpoint
  - EnvFilter   — default "info", env HFT_LOG override
```

non_blocking writer 의 핵심: hot thread 는 write 시 mpsc send 한 번만 하고 bg thread 가
실제 file/stdout 으로 flush. send 가 drop (queue full) 되면 counter 증가만.

### HDR Histogram
- 각 Stage 마다 `ArcSwap<Histogram<u64>>` (hdrhistogram crate).
- `record_stage_nanos` 는 내부적으로 `RwLock::read` 없이 thread-local sub-histogram 에 쌓고 주기적 (1s) 으로 global 에 merge. (lock-free record)
- `dump_hdr` 는 현재 snapshot clone 후 반환.

### verbose-trace feature
- `Cargo.toml` 에 `features = ["verbose-trace"]`. default 는 off.
- off 일 때 `tracing::debug!` / `trace!` 매크로가 `#[cfg]` 로 제거 (crate 내부 `trace_verbose!` 매크로 경유).

## 계약
- `init` 은 startup 에 1회. 중복 호출 시 `Err`.
- `TelemetryHandle` 을 drop 하면 OTel batch flush 보장 (최대 5s wait).
- hot path 는 `record_stage_nanos` 만 쓰고 다른 tracing 매크로는 `warn!`/`error!` 까지만.

## Phase 1 TODO

1. subscriber 조립 (fmt + otel + env_filter), non_blocking stdout writer.
2. HDR histogram 모듈 — stage 별 thread-local + global merge.
3. prometheus exporter — bg thread 가 scrape 서버 띄움 (TelemetryConfig.prom_port).
4. `pin_current_thread` (linux: `libc::sched_setaffinity`). `next_hot_core` 는 atomic counter 로 순차 부여.
5. `trace_verbose!` 매크로 — `#[cfg(feature = "verbose-trace")] tracing::trace!(...)` 로 확장.
6. stress test: hot thread 에서 10M 이벤트 찍으면서 `record_stage_nanos` 호출, p99.9 < 1μs 확인.

## Anti-jitter 체크
- stdout writer 는 반드시 non_blocking. blocking 쓰면 hot path 에서 mmap/write syscall spike.
- OTel batch processor 는 bg runtime 에서만. 핫 runtime 에서 돌면 안 됨.
- `metrics::counter!` macro 는 정적 문자열만 받음 → hot path 에서 format 방지.
- HDR 의 thread-local storage 는 `thread_local!` + `Cell`. lazy init.

## 완료 조건
- [ ] hot thread 에서 `record_stage_nanos` 10M 호출 p99.9 < 1μs
- [ ] OTel spans 가 bg 에서 batch export 되고 hot p99.9 에 영향 없음
- [ ] `cargo test --no-default-features -p hft-telemetry` 에서 debug! 매크로가 0 번 실행됨 (컴파일에서 제거 확인)
