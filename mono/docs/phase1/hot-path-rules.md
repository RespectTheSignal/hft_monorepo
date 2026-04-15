# Hot Path Rules — 핫패스에서 지켜야 할 규약

"hot path" = 거래소 WS 메시지 한 건을 받는 순간부터 subscriber/strategy 가 그 이벤트를
꺼내는 순간까지 실행되는 함수들. 이 경로는 **단일 쓰레드 관점에서 2ms 안에 끝나야 한다.**

## 반드시 지켜야 할 것 (MUST)

1. **할당 금지.** `Vec::new`, `String::new`, `Box::new`, `format!`, `.to_owned()` 사용 시 review 에서 reject.
2. **clone 최소화.** `.clone()` 은 작은 POD (< 64 byte) 한정. 큰 struct 는 `Arc` / move.
3. **panic 금지.** `unwrap`/`expect` 는 startup 에서만. hot path 는 `?` 또는 명시적 `match`.
4. **Clock 은 trait 경유.** `hft_time::Clock::now_ms()` / `now_nanos()` 만 사용.
5. **로그는 `warn!` 이상만.** `info!` 조차 hot path 안에서는 per-event 로 쓰지 말 것 (주기적 metrics 만).
6. **Config 는 Arc deref.** Config 필드 접근은 `self.cfg.field` 형태. 재계산 금지.
7. **ZMQ send 는 DONTWAIT.** blocking send 는 review 에서 reject.
8. **Async lock 금지.** `tokio::sync::Mutex` hot path 사용 금지. `parking_lot::Mutex` 또는 lock-free.

## 권장 (SHOULD)

- `#[inline]` 을 작은 hot 함수에 달아둔다 (bench 로 효과 확인 후).
- 분기 많은 match 는 `#[cold]` attribute 로 error 분기를 분리한다.
- atomic counter 는 `CachePadded` 로 감싼다.
- hot 구조체는 `#[repr(C)]` + 명시적 필드 순서로 layout 고정.
- `SmallVec<[T; N]>` 또는 `arrayvec::ArrayVec` 를 `Vec` 대신.

## 금지 목록 (반드시 아닌 경우 사용 금지)

```rust
// 이런 거 hot path 에 넣지 마세요
let msg = format!("symbol={}", sym);            // heap alloc + write
let v = vec![0u8; 120];                          // heap alloc
let s = json!({"a": 1}).to_string();             // heap alloc x 여러번
tokio::sync::Mutex::new(...).lock().await;       // await 는 재스케줄 가능
std::time::SystemTime::now();                    // mock 불가, 벤치 불가
tracing::debug!("received {:?}", event);         // release 에서도 compile 됨 + format
Regex::new("...").unwrap();                      // startup 아니면 금지
reqwest::get(...).await;                         // HTTP. bg 로 보내세요
```

## 허용 패턴 예시

### 버퍼 재사용
```rust
struct Publisher {
    buf: [u8; hft_protocol::wire::BOOK_TICKER_SIZE],  // stack-sized
    // or
    serialize_scratch: Vec<u8>,                        // pre-sized, reused
}

impl Publisher {
    #[inline]
    fn on_bookticker(&mut self, bt: &BookTicker, stamps: &mut LatencyStamps) {
        // self.buf 에 직접 씀. 할당 없음.
        hft_protocol::encode_bookticker(bt, stamps, &mut self.buf);
        stamps.serialized_ms = self.clock.now_ms();
        self.push_sock.send(&self.buf[..], ZMQ_DONTWAIT).ok();
        stamps.pushed_ms = self.clock.now_ms();
    }
}
```

### 토픽 사전계산
```rust
struct TopicCache {
    book_bytes: Arc<[u8]>,   // "gate_bookticker_BTC_USDT" bytes, startup 에 1회 계산
    trade_bytes: Arc<[u8]>,
}
```

### 에러 분기 cold
```rust
#[cold]
#[inline(never)]
fn handle_zmq_error(e: zmq::Error) {
    metrics::zmq_errors().increment(1);
    tracing::warn!(?e, "zmq send failed");
}
```

## 런타임 분리 규약

모든 서비스 바이너리의 `main` 은 두 개의 runtime 을 만든다:

```rust
fn main() -> anyhow::Result<()> {
    let cfg = hft_config::load_all()?;
    hft_telemetry::init(&cfg.service_name)?;

    let hot = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(cfg.hot_workers)        // 보통 거래소 WS 수와 일치
        .thread_name("hft-hot")
        .enable_io()
        .enable_time()
        .on_thread_start(|| {
            if let Some(core) = hft_telemetry::next_hot_core() {
                hft_telemetry::pin_current_thread(core);
            }
        })
        .build()?;

    let bg = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(cfg.bg_workers)          // 2~4 개
        .thread_name("hft-bg")
        .enable_all()
        .build()?;

    // hot runtime 에 들어가는 task:
    //   - exchange feed streams
    //   - publisher workers / aggregator
    //   - subscriber recv loop
    //   - strategy decision loop
    hot.block_on(run_hot(cfg.clone(), bg.handle().clone()))
}
```

`bg` handle 을 hot 쪽에 넘겨서, hot 에서 blocking 이 필요한 순간만 `bg.spawn(...)` 으로
넘긴다. 역방향 (bg → hot) 은 ZMQ 또는 channel 경유.

## Startup 순서 (서비스 공통)

1. `hft-config::load_all()` — env + file + supabase, 실패 시 panic OK
2. `hft-telemetry::init()` — subscriber, HDR histogram
3. `hft-zmq::Context::new()` — ZMQ context 1개 (프로세스당)
4. 거래소 구현체 생성 — `Box<dyn ExchangeFeed>`
5. runtime 2개 생성 (hot / bg)
6. `hft-testkit::WarmUp` 실행 (선택) — 파이프라인 warm 까지 대기
7. readiness 표시 → hot runtime 에서 실제 stream 시작

이 순서는 각 서비스 `SPEC.md` 의 "Startup 규약" 섹션에서 반복 언급된다.
